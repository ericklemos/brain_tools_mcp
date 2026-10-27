use brain_graph::assert_tool::AssertTool;
use brain_graph::builder::GraphBuilder;
use brain_graph::diff_tool::DiffTool;
use brain_graph::edit_tool::EditTool;
use brain_graph::patch_tool::PatchTool;
use brain_graph::write_tool::WriteTool;
use brain_graph::graph_tool::{GraphContextTool, GraphImpactTool, GraphHandler};
use brain_graph::history_tool::HistoryTool;
use brain_graph::map_tool::MapTool;
use brain_graph::read_tool::ReadTool;
use brain_graph::trace_tool::TraceTool;
use brain_graph::watcher::GraphWatcher;
use brain_engine::ValidationClient;
use brain_exec::{ExecTool, ShellTool};
use brain_kernel::deliveries::DeliveryLedger;
use brain_kernel::ledger_tool::LedgerTool;
use brain_kernel::peek_tool::PeekTool;
use brain_kernel::store::ResponseStore;
use brain_kernel::{Config, Registry};
use brain_search::SearchHandler;
use brain_tools::graph_sources::sources_from;
use brain_tools::mcp::server::McpServer;
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = Config::parse();
    // Canonicalize once: tools canonicalize the paths they read, so a relative
    // `--root-dir` (the default `.`) makes `strip_prefix` fail and headings pay
    // for an absolute path on every line.
    if let Ok(root) = config.root_dir.canonicalize() {
        config.root_dir = root;
    }

    // Direct all tracing/logging strictly to stderr
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        root_dir = %config.root_dir.display(),
        "Starting brain-tools MCP server daemon"
    );

    let resolved_model = config.resolved_model_path();
    if let Some(ref model_path) = resolved_model {
        tracing::info!(model_path = %model_path.display(), "Using GGUF model for validation");
    } else {
        tracing::warn!("No GGUF model configured or found; operating in lexical/AST fallback mode");
    }

    let validator = ValidationClient::new(resolved_model, config.gpu_layers)
        .with_max_candidates(config.max_validate_candidates);
    let store = Arc::new(ResponseStore::new(
        Duration::from_secs(config.handle_ttl_secs),
        config.handle_max_bytes,
    ));
    // Remembering what this session already delivered is the cheapest token:
    // the one not sent twice.
    let deliveries = config.delivery_ledger.then(|| {
        Arc::new(DeliveryLedger::new(
            config.delivery_entries,
            config.delivery_max_bytes,
        ))
    });
    let search_handler = Arc::new(
        SearchHandler::new(
            &config.root_dir,
            validator,
            config.default_max_results,
            config.default_min_confidence,
        )
        .with_budget(store.clone(), config.default_budget_tokens)
        .with_deliveries(deliveries.clone()),
    );

    // Initialize in-memory CodeGraph and Watcher
    let graph_builder = Arc::new(GraphBuilder::new()?);
    let code_graph = Arc::new(RwLock::new(graph_builder.build_graph(&config.root_dir)?));

    let _watcher = GraphWatcher::start(
        config.root_dir.clone(),
        code_graph.clone(),
        graph_builder.clone(),
    )?;

    let graph_handler = Arc::new(
        GraphHandler::new(&config.root_dir, code_graph.clone())
            .with_budget(store.clone(), config.default_budget_tokens),
    );

    // Tools are plugins: adding one is a new crate type plus one register call.
    let mut registry = Registry::new();
    registry
        .register(search_handler)
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(GraphContextTool::new(graph_handler.clone())))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(GraphImpactTool::new(graph_handler)))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(PeekTool::new(
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;

    // Appended last so the existing `tools/list` order is unchanged.
    if config.exec_disabled {
        tracing::info!("brain_exec disabled by configuration");
    } else {
        let exec_tool = ExecTool::new(
            &config.root_dir,
            store.clone(),
            config.default_budget_tokens,
            Duration::from_secs(config.exec_timeout_secs),
        )
        .with_sources(sources_from(code_graph.clone()));
        registry
            .register(Arc::new(exec_tool))
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    if config.shell_disabled {
        tracing::info!("brain_shell disabled by configuration");
    } else {
        registry
            .register(Arc::new(ShellTool::new(
                &config.root_dir,
                store.clone(),
                config.default_budget_tokens,
                Duration::from_secs(config.shell_timeout_secs),
            )))
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    // Orientation and history tools: they read the same watch-maintained graph.
    registry
        .register(Arc::new(
            ReadTool::new(
                &config.root_dir,
                store.clone(),
                config.default_budget_tokens,
            )
            .with_deliveries(deliveries.clone()),
        ))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(EditTool::new(
            &config.root_dir,
            code_graph.clone(),
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(WriteTool::new(&config.root_dir)))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(PatchTool::new(&config.root_dir)))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(MapTool::new(
            &config.root_dir,
            code_graph.clone(),
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(AssertTool::new(
            &config.root_dir,
            code_graph.clone(),
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(DiffTool::new(
            &config.root_dir,
            code_graph.clone(),
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(HistoryTool::new(
            &config.root_dir,
            code_graph.clone(),
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(LedgerTool::new(
            &config.root_dir,
            store.clone(),
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(TraceTool::new(
            &config.root_dir,
            code_graph,
            store,
            config.default_budget_tokens,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;

    // Schema tokens are paid on every request: a profile is the difference
    // between exposing 16 tools and exposing the 9 the session can use.
    if let Some(allowed) = config.tool_profile.tools() {
        registry.retain(allowed);
        tracing::info!(
            profile = ?config.tool_profile,
            tools = allowed.len(),
            "Tool profile applied"
        );
    }

    let server = McpServer::new(Arc::new(registry));

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    server.run_loop(stdin, stdout).await?;

    Ok(())
}
