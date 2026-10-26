use brain_tools::engine::ValidationClient;
use brain_tools::mcp::handlers::SearchHandler;
use brain_tools::mcp::server::{BrainToolDispatcher, McpServer};
use brain_tools::Config;
use clap::Parser;
use std::sync::Arc;
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

    let dispatcher = Arc::new(BrainToolDispatcher::new(search_handler));
    let server = McpServer::new(dispatcher);

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    server.run_loop(stdin, stdout).await?;

    Ok(())
}
