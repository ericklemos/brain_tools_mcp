use brain_exec::{ExecTool, ShellTool, ShellUsageLog};
use brain_graph::assert_tool::AssertTool;
use brain_graph::builder::GraphBuilder;
use brain_graph::diff_tool::DiffTool;
use brain_graph::edit_tool::EditTool;
use brain_graph::graph_tool::{GraphContextTool, GraphHandler, GraphImpactTool};
use brain_graph::history_tool::HistoryTool;
use brain_graph::map_tool::MapTool;
use brain_graph::patch_tool::PatchTool;
use brain_graph::read_tool::ReadTool;
use brain_graph::trace_tool::TraceTool;
use brain_graph::watcher::GraphWatcher;
use brain_graph::write_tool::WriteTool;
use brain_kernel::deliveries::DeliveryLedger;
use brain_kernel::ledger_tool::LedgerTool;
use brain_kernel::peek_tool::PeekTool;
use brain_kernel::store::ResponseStore;
use brain_kernel::{Config, Registry};
use brain_search::validator::ValidationClient;
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
        .with(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        root_dir = %config.root_dir.display(),
        "Starting brain-tools MCP server daemon"
    );

    // Ranking is lexical now: no model to load, no GPU to configure.
    let validator = ValidationClient::new().with_max_candidates(config.max_validate_candidates);
    let store = Arc::new(ResponseStore::new(
        Duration::from_secs(config.handle_ttl_secs),
        config.handle_max_bytes,
    ));

    // Local working memory: one append-only trail per session, under
    // `.brain/state`. It is independent of the delivery ledger: this one is
    // meant to survive the process.
    let state = config
        .state
        .then(|| Arc::new(brain_kernel::state::StateStore::open(&config.root_dir)));

    // brain_fetch and brain_websearch read the web the way brain_read reads
    // code: bounded blocks, a shared handle store, and no second download for
    // the same URL. They are created here because they borrow this process's
    // store; the registry, built below, adopts them.
    let web_fetcher: Arc<dyn brain_web::http::Fetcher> = Arc::new(
        brain_web::http::HttpFetcher::new(Duration::from_secs(20), 4 * 1024 * 1024)
            .expect("http client"),
    );
    let fetch_tool = Arc::new(
        brain_web::FetchTool::new(
            web_fetcher.clone(),
            config.root_dir.clone(),
            store.clone(),
        )
        .with_budget(config.default_budget_tokens),
    );
    brain_kernel::plugin::install(fetch_tool.clone());
    let providers: Vec<Arc<dyn brain_web::search::SearchProvider>> = vec![
        Arc::new(brain_web::search::duckduckgo::DuckDuckGo::new(
            web_fetcher.clone(),
        )),
        Arc::new(brain_web::search::marginalia::Marginalia::new(
            web_fetcher.clone(),
        )),
        Arc::new(brain_web::search::stackexchange::StackExchange::new(
            web_fetcher.clone(),
        )),
        Arc::new(brain_web::search::wikipedia::Wikipedia::new(
            web_fetcher.clone(),
        )),
    ];
    brain_kernel::plugin::install(Arc::new(
        brain_web::search::WebSearchTool::new(
            providers.clone(),
            config.root_dir.clone(),
            store.clone(),
        )
        .with_budget(900),
    ));
    brain_kernel::plugin::install(Arc::new(
        brain_web::search::research::ResearchTool::new(
            providers,
            fetch_tool,
            store.clone(),
        )
        .with_budget(1200),
    ));
    // Remembering what this session already delivered is the cheapest token:
    // the one not sent twice.
    let deliveries = config.delivery_ledger.then(|| {
        Arc::new(DeliveryLedger::new(
            config.delivery_entries,
            config.delivery_max_bytes,
        ))
    });
    // Usage telemetry for brain_shell: metadata by default, raw command and
    // output heads only with --shell-usage-log-full. Never fails a command.
    let shell_usage_log = (config.shell_usage_log && !config.shell_disabled).then(|| {
        let path = if config.shell_usage_log_path.is_absolute() {
            config.shell_usage_log_path.clone()
        } else {
            config.root_dir.join(&config.shell_usage_log_path)
        };
        Arc::new(ShellUsageLog::new(path, config.shell_usage_log_full))
    });
    if let Some(ref log) = shell_usage_log {
        tracing::info!(
            path = %log.path().display(),
            full = log.full(),
            "brain_shell usage log enabled"
        );
    }
    // Parsed once by the graph, reused by search and read: the second parse
    // of a matched file was the dominant cost of a search.
    let parse_cache = Arc::new(brain_kernel::parse_cache::ParseCache::new());
    let search_handler = Arc::new(
        SearchHandler::new(
            &config.root_dir,
            validator,
            config.default_max_results,
            config.default_min_confidence,
        )
        .with_budget(store.clone(), config.default_budget_tokens)
        .with_deliveries(deliveries.clone())
        .with_parse_cache(parse_cache.clone())
        .with_state(state.clone()),
    );

    // Initialize in-memory CodeGraph and Watcher
    let graph_builder = Arc::new(GraphBuilder::new()?.with_parse_cache(parse_cache.clone()));
    let code_graph = Arc::new(RwLock::new(graph_builder.build_graph(&config.root_dir)?));

    let _watcher = GraphWatcher::start(
        config.root_dir.clone(),
        code_graph.clone(),
        graph_builder.clone(),
    )?;

    let graph_handler = Arc::new(
        GraphHandler::new(&config.root_dir, code_graph.clone())
            .with_budget(store.clone(), config.default_budget_tokens)
            .with_state(state.clone()),
    );

    // Tools are plugins: adding one is a new crate type plus one register call.
    let mut registry = Registry::new();
    if let Some(ref state) = state {
        registry = registry.with_state(state.clone());
    }
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
            .register(Arc::new(
                ShellTool::new(
                    &config.root_dir,
                    store.clone(),
                    config.default_budget_tokens,
                    Duration::from_secs(config.shell_timeout_secs),
                )
                .with_guard(config.shell_guard)
                .with_usage_log(shell_usage_log.clone()),
            ))
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    // Orientation and history tools: they read the same watch-maintained graph.
    let read_tool = Arc::new(
        ReadTool::new(
            &config.root_dir,
            store.clone(),
            config.default_budget_tokens,
        )
        .with_deliveries(deliveries.clone()),
    );
    registry
        .register(read_tool.clone())
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(brain_graph::read_many_tool::ReadManyTool::new(
            read_tool,
        )))
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
        .register(Arc::new(brain_graph::write_many_tool::WriteManyTool::new(
            &config.root_dir,
        )))
        .map_err(|e| anyhow::anyhow!(e))?;
    registry
        .register(Arc::new(brain_graph::move_tool::MoveTool::new(
            &config.root_dir,
            code_graph.clone(),
        )))
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
        ).with_state(state.clone())))
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
        ).with_state(state.clone())))
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

    if let Some(ref state) = state {
        registry
            .register(Arc::new(brain_kernel::state_tool::StateTool::new(
                state.clone(),
                config.default_budget_tokens,
            )))
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    // Schema tokens are paid on every request: a profile is the difference
    // between exposing 16 tools and exposing the 9 the session can use.
    if let Some(allowed) = config.tool_profile.tools() {
        registry.retain(allowed);
        tracing::info!(
            profile = ?config.tool_profile,
            tools = allowed.len(),
            "Tool profile applied"
        );
    } else {
        // The setup tool documents the other tools, so it reads the registry
        // after the profile is applied: what it teaches is what this session
        // answers. It stays out of trimmed profiles — a read-only session
        // should not write into the project.
        let documented = brain_kernel::plugin::ToolDispatcher::list_tools(&registry);
        registry
            .register(Arc::new(brain_kernel::setup_tool::SetupTool::new(
                &config.root_dir,
                documented,
            )))
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    let server = McpServer::new(Arc::new(registry));

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    server.run_loop(stdin, stdout).await?;

    if let Some(ref state) = state {
        state.finish();
    }

    Ok(())
}
