use brain_graph::builder::GraphBuilder;
use brain_graph::graph_tool::{GraphContextTool, GraphImpactTool, GraphHandler};
use brain_graph::watcher::GraphWatcher;
use brain_engine::ValidationClient;
use brain_kernel::{Config, Registry};
use brain_search::SearchHandler;
use brain_tools::mcp::server::McpServer;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

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

    let validator = ValidationClient::new(resolved_model, config.gpu_layers);
    let search_handler = Arc::new(SearchHandler::new(
        &config.root_dir,
        validator,
        config.default_max_results,
        config.default_min_confidence,
    ));

    // Initialize in-memory CodeGraph and Watcher
    let graph_builder = Arc::new(GraphBuilder::new()?);
    let code_graph = Arc::new(RwLock::new(graph_builder.build_graph(&config.root_dir)?));

    let _watcher = GraphWatcher::start(
        config.root_dir.clone(),
        code_graph.clone(),
        graph_builder.clone(),
    )?;

    let graph_handler = Arc::new(GraphHandler::new(&config.root_dir, code_graph));

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

    let server = McpServer::new(Arc::new(registry));

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    server.run_loop(stdin, stdout).await?;

    Ok(())
}
