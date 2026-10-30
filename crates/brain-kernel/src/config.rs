use clap::Parser;
use std::path::PathBuf;

/// Which tools a client sees. Every exposed tool costs schema tokens on every
/// request, so a read-only or edit-only session should not pay for the rest.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProfile {
    Full,
    Core,
    Read,
}

impl ToolProfile {
    /// `None` means "no filtering" (full). The lists keep registration order
    /// because `Registry::retain` preserves it.
    pub fn tools(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Full => None,
            Self::Core => Some(&[
                "brain_search",
                "brain_context_graph",
                "brain_impact_check",
                "brain_peek",
                "brain_exec",
                "brain_read",
                "brain_read_many",
                "brain_edit",
                "brain_write",
                "brain_write_many",
                "brain_move",
                "brain_patch",
                "brain_map",
                "brain_research",
                "brain_websearch",
                "brain_fetch",
                "brain_state",
            ]),
            Self::Read => Some(&[
                "brain_search",
                "brain_context_graph",
                "brain_impact_check",
                "brain_peek",
                "brain_read",
                "brain_read_many",
                "brain_map",
                "brain_assert",
                "brain_diff",
                "brain_why",
                "brain_research",
                "brain_websearch",
                "brain_fetch",
                "brain_state",
            ]),
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "brain-tools",
    about = "High-performance local MCP server for surgical code search, context graph and impact analysis",
    version
)]
pub struct Config {
    /// Root directory of the repository to index and search
    #[arg(short = 'r', long, env = "BRAIN_ROOT_DIR", default_value = ".")]
    pub root_dir: PathBuf,

    /// Tool set exposed over MCP: full, core (edit loop) or read (review)
    #[arg(
        long = "tools",
        env = "BRAIN_TOOLS",
        value_enum,
        default_value = "full"
    )]
    pub tool_profile: ToolProfile,

    /// Number of threads for search
    #[arg(short = 't', long, env = "BRAIN_THREADS")]
    pub threads: Option<usize>,

    /// Log level filter (e.g. info, debug, warn, error)
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// Default minimum confidence threshold for brain_search (0.0 - 1.0)
    #[arg(long, env = "BRAIN_MIN_CONFIDENCE", default_value_t = 0.65)]
    pub default_min_confidence: f32,

    /// Default maximum results returned by brain_search
    #[arg(long, env = "BRAIN_MAX_RESULTS", default_value_t = 5)]
    pub default_max_results: usize,

    /// How many candidates the lexical ranker considers
    #[arg(long, env = "BRAIN_MAX_VALIDATE_CANDIDATES", default_value_t = 24)]
    pub max_validate_candidates: usize,

    /// Default approximate token budget for tool responses
    #[arg(long, env = "BRAIN_BUDGET_TOKENS", default_value_t = 2000)]
    pub default_budget_tokens: usize,

    /// Seconds a truncated response stays recoverable via brain_peek
    #[arg(long, env = "BRAIN_HANDLE_TTL_SECS", default_value_t = 1800)]
    pub handle_ttl_secs: u64,

    /// Total bytes the truncated-response store may hold
    #[arg(long, env = "BRAIN_HANDLE_MAX_BYTES", default_value_t = 67_108_864)]
    pub handle_max_bytes: usize,

    /// Remember what this session already delivered and point at it instead of
    /// sending the same body again (`--delivery-ledger false` disables it)
    #[arg(
        long,
        env = "BRAIN_DELIVERY_LEDGER",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub delivery_ledger: bool,

    /// Delivered spans the ledger remembers before evicting the oldest
    #[arg(long, env = "BRAIN_DELIVERY_ENTRIES", default_value_t = 512)]
    pub delivery_entries: usize,

    /// Bytes of delivered text the ledger may keep for pointers and diffs
    #[arg(long, env = "BRAIN_DELIVERY_MAX_BYTES", default_value_t = 8_388_608)]
    pub delivery_max_bytes: usize,

    /// Keep a local session trail under `.brain/state` (events, notes, tasks)
    #[arg(
        long,
        env = "BRAIN_STATE",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub state: bool,

    /// Kill a brain_exec run after this many seconds and return what it captured
    #[arg(long, env = "BRAIN_EXEC_TIMEOUT_SECS", default_value_t = 300)]
    pub exec_timeout_secs: u64,

    /// Do not register brain_exec, for hosts where spawning processes is not acceptable
    #[arg(long, env = "BRAIN_EXEC_DISABLED", default_value_t = false)]
    pub exec_disabled: bool,

    /// Kill a brain_shell command after this many seconds
    #[arg(long, env = "BRAIN_SHELL_TIMEOUT_SECS", default_value_t = 120)]
    pub shell_timeout_secs: u64,

    /// Do not register brain_shell, for hosts where arbitrary commands are not acceptable
    #[arg(long, env = "BRAIN_SHELL_DISABLED", default_value_t = false)]
    pub shell_disabled: bool,

    /// Refuse brain_shell commands that a brain tool already covers
    #[arg(
        long,
        env = "BRAIN_SHELL_GUARD",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub shell_guard: bool,

    /// Write brain_shell usage metadata to a local JSONL file (`false` disables)
    #[arg(
        long,
        env = "BRAIN_SHELL_USAGE_LOG",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub shell_usage_log: bool,

    /// Path of the brain_shell usage log, relative to --root-dir
    #[arg(
        long,
        env = "BRAIN_SHELL_USAGE_LOG_PATH",
        default_value = ".brain/shell-usage.jsonl"
    )]
    pub shell_usage_log_path: PathBuf,

    /// Include the raw command and the first 200 chars of stdout/stderr in the log
    #[arg(long, env = "BRAIN_SHELL_USAGE_LOG_FULL", default_value_t = false)]
    pub shell_usage_log_full: bool,
}
