use clap::Parser;
use std::path::PathBuf;

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

    /// Path to GGUF model file (e.g. models/vllm-d3-edge-Q8_0.gguf)
    #[arg(short = 'm', long, env = "BRAIN_MODEL_PATH")]
    pub model_path: Option<PathBuf>,

    /// Number of GPU layers to offload to CUDA/Vulkan/Metal
    #[arg(long, env = "BRAIN_GPU_LAYERS", default_value_t = 33)]
    pub gpu_layers: u32,

    /// Number of threads for CPU inference and search
    #[arg(short = 't', long, env = "BRAIN_THREADS")]
    pub threads: Option<usize>,

    /// Log level filter (e.g. info, debug, warn, error)
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// Default minimum confidence threshold for model validation (0.0 - 1.0)
    #[arg(long, env = "BRAIN_MIN_CONFIDENCE", default_value_t = 0.65)]
    pub default_min_confidence: f32,

    /// Default maximum results returned by brain_search
    #[arg(long, env = "BRAIN_MAX_RESULTS", default_value_t = 5)]
    pub default_max_results: usize,

    /// Default approximate token budget for tool responses
    #[arg(long, env = "BRAIN_BUDGET_TOKENS", default_value_t = 2000)]
    pub default_budget_tokens: usize,

    /// Seconds a truncated response stays recoverable via brain_peek
    #[arg(long, env = "BRAIN_HANDLE_TTL_SECS", default_value_t = 1800)]
    pub handle_ttl_secs: u64,

    /// Total bytes the truncated-response store may hold
    #[arg(long, env = "BRAIN_HANDLE_MAX_BYTES", default_value_t = 67_108_864)]
    pub handle_max_bytes: usize,
}

impl Config {
    /// Resolves default model path: if not explicitly provided, checks if models/vllm-d3-edge-Q8_0.gguf exists
    pub fn resolved_model_path(&self) -> Option<PathBuf> {
        if let Some(ref path) = self.model_path {
            return Some(path.clone());
        }

        let default_candidate = PathBuf::from("models/vllm-d3-edge-Q8_0.gguf");
        if default_candidate.exists() {
            Some(default_candidate)
        } else {
            None
        }
    }
}
