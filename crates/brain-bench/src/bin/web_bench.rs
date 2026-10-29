//! Web benchmark: bounded evidence vs the whole page.
//!
//! Both sides go through the same extractor, so the number isolates the
//! variable this project claims to change: selection and budget. The "full"
//! side is what a fetch tool returns — every block of the page.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use brain_kernel::budget::estimate_tokens;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Parser)]
struct Cli {
    /// Servidor MCP a medir
    #[arg(long, default_value = "target/debug/brain-tools")]
    server: PathBuf,

    /// Corpus de URLs
    #[arg(long, default_value = "bench/web.json")]
    corpus: PathBuf,

    /// Raiz passada ao servidor (cache e store ficam aqui)
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Teto de tokens do lado "bounded"
    #[arg(long, default_value_t = 800)]
    budget: usize,

    /// Segundos de teto por chamada MCP
    #[arg(long, default_value_t = 120)]
    timeout_secs: u64,

    /// Escreve o resultado em JSON
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    id: String,
    url: String,
    question: String,
    /// Trecho que precisa sobreviver ao corte. Se não estiver nem na página
    /// inteira, o corpus está errado, não a ferramenta.
    expect: String,
}

#[derive(Debug, Serialize)]
struct Row {
    id: String,
    url: String,
    ok: bool,
    full_has_expect: bool,
    bounded_has_expect: bool,
    full_tokens: usize,
    bounded_tokens: usize,
    ratio: f64,
    network_ms: u64,
    cache_ms: u64,
    error: Option<String>,
}

struct Client {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
}

impl Client {
    async fn spawn(server: &Path, root: &Path, timeout: Duration) -> Result<Self> {
        let mut child = Command::new(server)
            .arg("--root-dir")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {}", server.display()))?;
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut client = Self {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
            timeout,
        };
        client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }),
            )
            .await?;
        Ok(client)
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let mut response = String::new();
        let read = tokio::time::timeout(self.timeout, self.stdout.read_line(&mut response)).await;
        match read {
            Ok(Ok(0)) => Err(anyhow!("servidor encerrou stdout")),
            Ok(Ok(_)) => {
                let value: serde_json::Value = serde_json::from_str(response.trim())?;
                if let Some(error) = value.get("error") {
                    return Err(anyhow!("JSON-RPC error: {error}"));
                }
                Ok(value.get("result").cloned().unwrap_or_default())
            }
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(anyhow!("timeout de {}s", self.timeout.as_secs())),
        }
    }

    async fn call(&mut self, name: &str, arguments: serde_json::Value) -> Result<(String, bool)> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let text = result
            .get("content")
            .and_then(|content| content.get(0))
            .and_then(|first| first.get("text"))
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .to_string();
        let is_error = result
            .get("isError")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        Ok((text, is_error))
    }
}

fn has(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * p).round() as usize;
    values[index]
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let raw = std::fs::read_to_string(&cli.corpus)
        .with_context(|| format!("corpus {}", cli.corpus.display()))?;
    let entries: Vec<Entry> = serde_json::from_str(&raw)?;
    if entries.is_empty() {
        return Err(anyhow!("corpus vazio"));
    }

    let root = cli.root.canonicalize()?;
    let mut client = Client::spawn(&cli.server, &root, Duration::from_secs(cli.timeout_secs)).await?;

    let mut rows = Vec::with_capacity(entries.len());
    for entry in &entries {
        let bounded_args = serde_json::json!({
            "url": entry.url,
            "question": entry.question,
            "budget_tokens": cli.budget,
        });

        let started = Instant::now();
        let (bounded, bounded_error) = client
            .call("brain_fetch", bounded_args.clone())
            .await
            .unwrap_or_else(|error| (error.to_string(), true));
        let network_ms = started.elapsed().as_millis() as u64;

        // Same extractor, no question, no budget: what a fetch tool delivers.
        let (full, full_error) = client
            .call(
                "brain_fetch",
                serde_json::json!({ "url": entry.url, "budget_tokens": 1_000_000usize }),
            )
            .await
            .unwrap_or_else(|error| (error.to_string(), true));

        let started = Instant::now();
        let (re_read, re_error) = client
            .call("brain_fetch", bounded_args)
            .await
            .unwrap_or_else(|error| (error.to_string(), true));
        let cache_ms = started.elapsed().as_millis() as u64;

        let error = bounded_error
            .then(|| bounded.clone())
            .or_else(|| full_error.then(|| full.clone()))
            .or_else(|| re_error.then(|| re_read.clone()));

        let full_tokens = estimate_tokens(&full);
        let bounded_tokens = estimate_tokens(&bounded);
        rows.push(Row {
            id: entry.id.clone(),
            url: entry.url.clone(),
            ok: error.is_none(),
            full_has_expect: has(&full, &entry.expect),
            bounded_has_expect: has(&bounded, &entry.expect),
            full_tokens,
            bounded_tokens,
            ratio: if bounded_tokens == 0 {
                0.0
            } else {
                full_tokens as f64 / bounded_tokens as f64
            },
            network_ms,
            cache_ms,
            error,
        });
    }

    let valid: Vec<&Row> = rows
        .iter()
        .filter(|row| row.ok && row.full_has_expect)
        .collect();
    let total_full: usize = valid.iter().map(|row| row.full_tokens).sum();
    let total_bounded: usize = valid.iter().map(|row| row.bounded_tokens).sum();
    let recall = valid.iter().filter(|row| row.bounded_has_expect).count();
    let mut network: Vec<u64> = rows.iter().map(|row| row.network_ms).collect();
    let mut cache: Vec<u64> = rows.iter().map(|row| row.cache_ms).collect();

    println!(
        "| id | full tok | bounded tok | ratio | recall | net ms | cache ms |"
    );
    println!("|---|---:|---:|---:|:--:|---:|---:|");
    for row in &rows {
        println!(
            "| {} | {} | {} | {:.1}x | {} | {} | {} |",
            row.id,
            row.full_tokens,
            row.bounded_tokens,
            row.ratio,
            if !row.ok {
                "erro"
            } else if !row.full_has_expect {
                "corpus"
            } else if row.bounded_has_expect {
                "sim"
            } else {
                "NÃO"
            },
            row.network_ms,
            row.cache_ms,
        );
    }

    println!();
    println!("URLs: {} · válidas: {}", rows.len(), valid.len());
    println!(
        "tokens: full {} · bounded {} · redução {:.1}x",
        total_full,
        total_bounded,
        if total_bounded == 0 {
            0.0
        } else {
            total_full as f64 / total_bounded as f64
        }
    );
    println!(
        "recall da evidência com orçamento: {}/{} ({:.0}%)",
        recall,
        valid.len(),
        if valid.is_empty() {
            0.0
        } else {
            recall as f64 / valid.len() as f64 * 100.0
        }
    );
    println!(
        "latência: p50 rede {} ms · p95 rede {} ms · p50 cache {} ms",
        percentile(&mut network, 0.5),
        percentile(&mut network, 0.95),
        percentile(&mut cache, 0.5)
    );

    if let Some(path) = &cli.json {
        let body = serde_json::to_string_pretty(&rows)?;
        std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    }

    let failures = rows.iter().filter(|row| !row.ok).count();
    if failures > 0 {
        println!("falhas de rede/erro: {failures}");
    }
    Ok(())
}
