//! CLI: replay the fixed tasks, print the cost table, guard against regressions.

use anyhow::{bail, Context, Result};
use brain_bench::client::McpClient;
use brain_bench::naive;
use brain_bench::report::{self, CallRun, RunReport, SchemaRun, TaskRun, REPORT_VERSION};
use brain_bench::tasks::TaskSet;
use brain_kernel::budget::estimate_tokens;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "brain-bench",
    about = "Mede quantos tokens o brain-tools gasta para responder perguntas fixas, comparado com reler os arquivos"
)]
struct Cli {
    /// Servidor MCP a medir
    #[arg(long, default_value = "target/release/brain-tools")]
    server: PathBuf,

    /// Raiz do repositório medido (passada ao servidor como --root-dir)
    #[arg(long, short = 'r', default_value = ".")]
    root: PathBuf,

    /// Arquivo de tarefas
    #[arg(long, default_value = "bench/tasks.json")]
    tasks: PathBuf,

    /// Baseline gravado por --update
    #[arg(long, default_value = "bench/baseline.json")]
    baseline: PathBuf,

    /// Grava o resultado atual como baseline
    #[arg(long)]
    update: bool,

    /// Falha quando uma tarefa (ou o schema) gastar mais que N% acima do baseline
    #[arg(long, default_value_t = 10.0)]
    max_growth: f64,

    /// Escreve o relatório completo em JSON
    #[arg(long)]
    json: Option<PathBuf>,

    /// Segundos de teto por chamada MCP
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,

    /// Repassa um argumento extra ao servidor (repetível)
    #[arg(long = "server-arg", value_name = "ARG")]
    server_args: Vec<String>,

    /// Mostra o stderr do servidor
    #[arg(long, short = 'v')]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let server = resolve_server(&cli.server)?;
    // The report keeps the root as the caller wrote it ("."), not the
    // canonical path: the baseline is committed and must not carry a machine's
    // home directory.
    let root_label = cli.root.display().to_string();
    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("raiz inválida: {}", cli.root.display()))?;

    let tasks = TaskSet::load(&cli.tasks)?;

    // Baseline first: a typo in the task file must fail before a server is
    // spawned, not halfway through the run.
    let mut baselines = Vec::with_capacity(tasks.tasks.len());
    for task in &tasks.tasks {
        let cost =
            naive::measure(&root, &task.naive).with_context(|| format!("tarefa `{}`", task.id))?;
        baselines.push(cost);
    }

    let mut client = McpClient::spawn(
        &server,
        &root,
        &cli.server_args,
        Duration::from_secs(cli.timeout_secs),
        cli.verbose,
    )
    .await?;
    client.initialize().await?;
    let (schema_json, tool_count) = client.list_tools().await?;
    let schema = SchemaRun {
        tools: tool_count,
        chars: schema_json.chars().count(),
        tokens: estimate_tokens(&schema_json),
    };

    let mut run = Vec::with_capacity(tasks.tasks.len());
    for (task, naive_cost) in tasks.tasks.iter().zip(baselines.iter()) {
        let mut calls = Vec::with_capacity(task.calls.len());
        let mut error = None;
        let started = Instant::now();

        for call in &task.calls {
            let request = serde_json::json!({ "name": call.tool, "arguments": call.arguments });
            let request_tokens = estimate_tokens(&serde_json::to_string(&request)?);
            let call_started = Instant::now();
            let outcome = match client.call_tool(&call.tool, call.arguments.clone()).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    error = Some(format!("`{}`: {err}", call.tool));
                    break;
                }
            };
            let is_error = outcome.is_error;
            calls.push(CallRun {
                tool: call.tool.clone(),
                request_tokens,
                response_tokens: estimate_tokens(&outcome.text),
                chars: outcome.text.chars().count(),
                is_error,
                millis: call_started.elapsed().as_millis() as u64,
            });
            if is_error {
                error = Some(format!(
                    "`{}` respondeu com erro: {}",
                    call.tool,
                    first_line(&outcome.text)
                ));
                break;
            }
        }

        let brain_tokens = calls.iter().map(|c| c.tokens()).sum();
        let brain_chars = calls.iter().map(|c| c.chars).sum();
        run.push(TaskRun {
            id: task.id.clone(),
            question: task.question.clone(),
            calls,
            brain_tokens,
            brain_chars,
            naive_tokens: naive_cost.tokens,
            naive_bytes: naive_cost.bytes,
            naive_files: naive_cost.paths(),
            millis: started.elapsed().as_millis() as u64,
            error,
        });
    }
    client.shutdown().await;

    let report = RunReport {
        version: REPORT_VERSION,
        server: cli.server.display().to_string(),
        root: root_label,
        schema,
        tasks: run,
    };

    println!("{}", report::render(&report));

    if let Some(path) = &cli.json {
        write_file(path, &report.to_json()?)?;
        eprintln!("relatório json: {}", path.display());
    }

    if cli.update {
        // A baseline with a failed task would freeze the failure as the
        // reference and make every later run look clean.
        if let Some(failed) = report.tasks.iter().find(|t| !t.ok()) {
            bail!(
                "não gravo baseline com tarefa falhando: `{}` — {}",
                failed.id,
                failed.error.as_deref().unwrap_or("erro desconhecido")
            );
        }
        write_file(&cli.baseline, &report.to_json()?)?;
        println!("baseline atualizado: {}", cli.baseline.display());
        return Ok(());
    }

    if !cli.baseline.exists() {
        println!(
            "sem baseline em {} — rode com --update para gravar",
            cli.baseline.display()
        );
        return Ok(());
    }

    let baseline_raw = std::fs::read_to_string(&cli.baseline)
        .with_context(|| format!("falha ao ler o baseline {}", cli.baseline.display()))?;
    let baseline: RunReport = serde_json::from_str(&baseline_raw)
        .with_context(|| format!("baseline ilegível em {}", cli.baseline.display()))?;
    if baseline.version != REPORT_VERSION {
        bail!(
            "baseline no formato v{}, relatório atual no formato v{}: regrave com --update",
            baseline.version,
            REPORT_VERSION
        );
    }

    let comparison = report::compare(&baseline, &report, cli.max_growth);
    for id in &comparison.new_tasks {
        println!("tarefa nova, fora do baseline: {id}");
    }
    for id in &comparison.missing_tasks {
        println!("tarefa removida do arquivo de tarefas, ainda no baseline: {id}");
    }
    if comparison.regressions.is_empty() {
        println!(
            "sem regressão: nenhuma tarefa gastou mais de {:.0}% acima do baseline",
            cli.max_growth
        );
        return Ok(());
    }

    for regression in &comparison.regressions {
        println!(
            "REGRESSÃO [{}] {}: {} → {} tokens ({:+.1}%)",
            regression.scope,
            regression.task,
            regression.baseline_tokens,
            regression.current_tokens,
            regression.percent
        );
    }
    std::process::exit(1);
}

fn resolve_server(requested: &Path) -> Result<PathBuf> {
    if requested.exists() {
        return Ok(requested.to_path_buf());
    }
    if requested == Path::new("target/release/brain-tools") {
        let debug = Path::new("target/debug/brain-tools");
        if debug.exists() {
            eprintln!(
                "aviso: {} não existe; medindo o binário de debug {} (build mais lento, mesmo conteúdo)",
                requested.display(),
                debug.display()
            );
            return Ok(debug.to_path_buf());
        }
    }
    bail!(
        "binário do servidor não encontrado em {}: compile com `cargo build --release` ou passe --server",
        requested.display()
    )
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("falha ao criar {}", parent.display()))?;
        }
    }
    std::fs::write(path, contents).with_context(|| format!("falha ao escrever {}", path.display()))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}
