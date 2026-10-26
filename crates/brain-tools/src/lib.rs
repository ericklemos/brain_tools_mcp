//! Compatibility facade over the microkernel workspace.
//!
//! The library used to be one crate with six modules. The code now lives in
//! `brain-kernel`, `brain-search`, `brain-graph` and `brain-engine`; the paths
//! below re-export it so existing callers and the integration tests keep
//! compiling unchanged.

pub use brain_kernel::{error, BrainError, Config, Result};

mod mcp_server;

pub mod mcp {
    pub use brain_kernel::protocol;
    pub use brain_kernel::plugin::{EmptyDispatcher, Registry, ToolDispatcher, ToolPlugin};

    pub mod server {
        pub use brain_kernel::plugin::{EmptyDispatcher, ToolDispatcher};
        pub use crate::mcp_server::{BrainToolDispatcher, McpServer};
    }

    pub mod handlers {
        pub use brain_graph::graph_tool::{GraphContextTool, GraphHandler, GraphImpactTool};
        pub use brain_search::search_tool::{BrainSearchParams, SearchHandler};
    }

    pub mod schemas {
        pub use brain_graph::schema::{brain_context_graph_tool, brain_impact_check_tool};
        pub use brain_search::schema::brain_search_tool;
    }
}

pub mod search {
    pub use brain_kernel::{ast as ast_chunker, ranking as ranker};
    pub use brain_search::scanner;
}

pub mod graph {
    pub use brain_graph::{builder, model, queries, traversal, watcher};
}

pub mod engine {
    pub use brain_engine::{actor, llama_validator, prompt};
    pub use brain_engine::{LlamaValidator, PromptBuilder, ValidationClient, ValidationJob};
}

/// Adapter wiring the code graph's hashes into `brain_exec`'s cache.
pub mod graph_sources;

pub mod exec {
    pub use brain_exec::{ExecParams, ExecTool, Kind, SourceHashes};
}
