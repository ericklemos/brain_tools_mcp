use brain_tools::Config;
use clap::Parser;
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

    if let Some(model_path) = config.resolved_model_path() {
        tracing::info!(model_path = %model_path.display(), "Using GGUF model for validation");
    } else {
        tracing::warn!("No GGUF model configured or found; operating in lexical/AST fallback mode");
    }

    Ok(())
}
