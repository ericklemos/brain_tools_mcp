# Microkernel Workspace Split — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the single `brain-tools` crate into a workspace (`brain-kernel`, `brain-search`, `brain-graph`, `brain-engine`, `brain-tools`) where MCP tools register through a `ToolPlugin` trait instead of a hardcoded `match`.

**Architecture:** `brain-kernel` holds protocol types, shared AST types, `AstChunker`, `Config`, `BrainError` and the plugin registry. Each tool crate depends only on the kernel. The binary crate mounts everything and re-exports the old module paths so the 11 integration tests keep compiling untouched.

**Tech Stack:** Rust 2021, Cargo workspace, tokio, tree-sitter, petgraph, llama-cpp-2 (isolated in `brain-engine`).

**Spec:** `docs/superpowers/specs/2026-10-26-microkernel-workspace-design.md`

## Global Constraints

- **No behavior change.** This is a structural move only. Search, graph and inference semantics must be byte-identical afterwards.
- **Nothing the spec calls "fora de escopo"** may be added: no test-file filtering, no `brain_list_files`.
- **All 11 integration tests** in `tests/` must pass **without any edit to their `use` lines**. That is the compatibility proof.
- **`brain-kernel` must not depend on `llama-cpp-2`, `petgraph`, `notify`, `ignore`, `grep-searcher`, or any tool crate.**
- **Feature propagation:** `cuda`/`vulkan`/`metal` must reach `llama-cpp-2` through `brain-tools` → `brain-engine`.
- **`git mv`** for every moved source file so history follows.
- Every task ends green on `cargo test` before its commit.

## Review Focus

- **`llama-cpp-2` feature propagation breaking only the GPU build.** A plain `cargo build` can pass while `--features cuda` fails. Task 8 must build both.
- **Crate-name collision with the existing binary.** Package `brain-tools` produces bin `brain-tools`; the workspace directory is also `brain-tools`. Task 1 must verify `cargo run` still starts the server.
- **`ast_chunker` placed in the wrong crate** recreating a sibling dependency (`brain-graph → brain-search`). It belongs in the kernel; Task 2 pins this.
- **Silent protocol drift** from `handlers.rs` becoming a trait object — e.g. a tool vanishing from `list_tools()`. Task 5 asserts all three schemas are still listed.
- **Registry name collisions** (two plugins claiming one name) failing silently at runtime instead of startup. Task 4 pins fail-fast behavior.

---

### Task 1: Create the workspace skeleton

**Files:**
- Create: `Cargo.toml` (workspace root, replacing the current package manifest)
- Create: `crates/brain-kernel/Cargo.toml`, `crates/brain-search/Cargo.toml`, `crates/brain-graph/Cargo.toml`, `crates/brain-engine/Cargo.toml`, `crates/brain-tools/Cargo.toml`
- Move: `Cargo.lock` stays at root

**Interfaces:**
- Consumes: nothing.
- Produces: workspace with five members; `brain-tools` still builds and runs the current binary unchanged.

- [ ] **Step 1: Record the current baseline**

```bash
cd /home/desktop/Documents/projects/brain_tools
cargo test 2>&1 | grep -E "^test result" | tail -20
git rev-parse HEAD
```

Expected: all suites pass. Note the commit SHA — this is the rollback point.

- [ ] **Step 2: Write the workspace root `Cargo.toml`**

Replace the package manifest with a virtual workspace manifest, keeping `[workspace.dependencies]` for every third-party crate currently listed, with the exact version strings copied from the old `[dependencies]`.

```toml
[workspace]
resolver = "2"
members = [
    "crates/brain-kernel",
    "crates/brain-search",
    "crates/brain-graph",
    "crates/brain-engine",
    "crates/brain-tools",
]

[workspace.package]
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
tokio = { version = "1.40", features = ["full"] }
tokio-util = { version = "0.7", features = ["codec"] }
crossbeam-channel = "0.5"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4.5", features = ["derive", "env"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
thiserror = "1.0"
anyhow = "1.0"
regex = "1.10"
ignore = "0.4"
grep-searcher = "0.1"
grep-regex = "0.1"
grep-matcher = "0.1"
tree-sitter = "0.24"
tree-sitter-rust = "0.23"
tree-sitter-kotlin-ng = "1.1.0"
streaming-iterator = "0.1"
petgraph = { version = "0.6", features = ["serde-1"] }
blake3 = "1.5"
notify = "6.1"
llama-cpp-2 = { version = "0.1.160", default-features = false, features = ["common"] }
tempfile = "3.10"
```

Note `[workspace.package]`, so each member can write `version.workspace = true` and `edition.workspace = true` instead of repeating them.

- [ ] **Step 3: Create each member manifest depending only on what it needs**

Versions below are copied verbatim from the current root manifest — do not bump them.

`crates/brain-kernel/Cargo.toml`:

```toml
[package]
name = "brain-kernel"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
clap = { workspace = true }
tree-sitter = { workspace = true }
tree-sitter-rust = { workspace = true }
tree-sitter-kotlin-ng = { workspace = true }
```

`streaming-iterator` is **not** a kernel dependency — the only user is `graph/builder.rs` (it consumes tree-sitter cursors). It belongs to `brain-graph`.

`crates/brain-search/Cargo.toml`: adds `brain-kernel = { path = "../brain-kernel" }`, plus `ignore`, `grep-searcher`, `grep-regex`, `grep-matcher`, `regex`, `tokio`, `tracing` — all `{ workspace = true }`.

`crates/brain-graph/Cargo.toml`: `brain-kernel`, plus `petgraph`, `notify`, `blake3`, `serde`, `serde_json`, `tokio`, `tracing`, `ignore`, `tree-sitter`, `streaming-iterator` — **all `{ workspace = true }`**. `builder.rs` parses ASTs directly and `watcher.rs` uses `notify`/`blake3`, so these are not transitive.

`crates/brain-engine/Cargo.toml`: `brain-kernel`, plus `llama-cpp-2` (workspace, `default-features = false`, `features = ["common"]`), `crossbeam-channel`, `tokio`, `tracing`.

`crates/brain-tools/Cargo.toml`: `brain-kernel`, `brain-search`, `brain-graph`, `brain-engine` as path deps, plus `tokio`, `tokio-util`, `serde`, `serde_json`, `anyhow`, `tracing`, `tracing-subscriber`, `clap`.

```toml
[[bin]]
name = "brain-tools"
path = "src/main.rs"
```

`brain-tools` alone declares the features:

```toml
[features]
default = []
cuda = ["brain-engine/cuda"]
vulkan = ["brain-engine/vulkan"]
metal = ["brain-engine/metal"]
```

and `brain-engine` declares:

```toml
[features]
default = []
cuda = ["llama-cpp-2/cuda"]
vulkan = ["llama-cpp-2/vulkan"]
metal = ["llama-cpp-2/metal"]
```

`dashmap` is currently declared but imported nowhere in `src/` — drop it from the workspace rather than carrying a dead dependency into `brain-tools`.

- [ ] **Step 4: Move the integration tests and their dev-dependency**

A virtual workspace has no package of its own, so the root `tests/` directory would be orphaned and silently stop running. Move it into the binary crate, which is the only member that still produces the `brain-tools` binary the smoke test spawns via `CARGO_BIN_EXE_brain-tools`:

```bash
mkdir -p crates/brain-tools/tests
git mv tests/*.rs crates/brain-tools/tests/
rmdir tests
```

Add to `crates/brain-tools/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 5: Verify the skeleton resolves**

```bash
cargo metadata --format-version 1 > /dev/null && echo "workspace OK"
```

Expected: prints `workspace OK`. (Compilation fails at this point — `src/` still lives at root; Task 2 moves it.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/
git commit -m "build: scaffold microkernel workspace with five crates"
```

---

### Task 2: Move code into `brain-kernel`

**Files:**
- Move: `src/error.rs` → `crates/brain-kernel/src/error.rs`
- Move: `src/config.rs` → `crates/brain-kernel/src/config.rs`
- Move: `src/mcp/protocol.rs` → `crates/brain-kernel/src/protocol.rs`
- Move: `src/search/ast_chunker.rs` → `crates/brain-kernel/src/ast.rs`
- Move: `src/search/ranker.rs` → `crates/brain-kernel/src/ranking.rs`
- Create: `crates/brain-kernel/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's manifests.
- Produces: `brain_kernel::{BrainError, Result, Config, protocol::*, ast::{SupportedLanguage, AstSnippet, AstChunker}, ranking::{ScoredSnippet, LexicalRanker}}`.

- [ ] **Step 1: Move the files**

```bash
mkdir -p crates/brain-kernel/src
git mv src/error.rs            crates/brain-kernel/src/error.rs
git mv src/config.rs           crates/brain-kernel/src/config.rs
git mv src/mcp/protocol.rs     crates/brain-kernel/src/protocol.rs
git mv src/search/ast_chunker.rs crates/brain-kernel/src/ast.rs
git mv src/search/ranker.rs    crates/brain-kernel/src/ranking.rs
```

- [ ] **Step 2: Write `crates/brain-kernel/src/lib.rs`**

```rust
pub mod ast;
pub mod config;
pub mod error;
pub mod protocol;
pub mod ranking;

pub use ast::{AstChunker, AstSnippet, SupportedLanguage};
pub use config::Config;
pub use error::{BrainError, Result};
pub use ranking::{LexicalRanker, ScoredSnippet};
```

- [ ] **Step 3: Rewrite internal `use crate::...` paths in the moved files**

In `ast.rs` and `ranking.rs`, `use crate::error::{BrainError, Result}` still resolves (same crate). In `ranking.rs`, change `use crate::search::ast_chunker::AstSnippet;` to `use crate::ast::AstSnippet;`.

- [ ] **Step 4: Verify the kernel compiles and its unit tests pass**

```bash
cargo test -p brain-kernel 2>&1 | grep -E "^test result|error\["
```

Expected: `AstChunker` and `LexicalRanker` unit tests pass; no compile errors.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(kernel): move shared types, config, protocol and AST parser into brain-kernel"
```

---

### Task 3: Move tool code into `brain-search`, `brain-graph`, `brain-engine`

**Files:**
- Move: `src/search/scanner.rs` → `crates/brain-search/src/scanner.rs`
- Move: `src/mcp/handlers.rs` → `crates/brain-search/src/search_tool.rs` **and** `crates/brain-graph/src/graph_tool.rs` (split — see step 2)
- Move: `src/graph/{builder,model,queries,traversal,watcher}.rs` → `crates/brain-graph/src/`
- Move: `src/engine/{actor,llama_validator,prompt}.rs` → `crates/brain-engine/src/`
- Move: `src/mcp/schemas.rs` → split into `crates/brain-search/src/schema.rs` and `crates/brain-graph/src/schema.rs`

**Interfaces:**
- Consumes: `brain_kernel::*` from Task 2.
- Produces: `brain_search::SearchHandler`, `brain_graph::GraphHandler`, `brain_engine::ValidationClient` — same signatures as today, now across crate boundaries.

- [ ] **Step 1: Move scanner, graph and engine files**

```bash
mkdir -p crates/brain-search/src crates/brain-graph/src crates/brain-engine/src
git mv src/search/scanner.rs crates/brain-search/src/scanner.rs
for f in builder model queries traversal watcher; do
  git mv src/graph/$f.rs crates/brain-graph/src/$f.rs
done
for f in actor llama_validator prompt; do
  git mv src/engine/$f.rs crates/brain-engine/src/$f.rs
done
```

- [ ] **Step 2: Split `handlers.rs` into two tool-crate files**

`SearchHandler` + `BrainSearchParams` + `handle_brain_search` go to `crates/brain-search/src/search_tool.rs`.
`GraphHandler` + `handle_context_graph` + `handle_impact_check` go to `crates/brain-graph/src/graph_tool.rs`.

Do the split by hand (create two files, delete the original), because `git mv` cannot split.

- [ ] **Step 3: Split `schemas.rs` the same way**

`brain_search_tool()` → `crates/brain-search/src/schema.rs`.
`brain_context_graph_tool()` and `brain_impact_check_tool()` → `crates/brain-graph/src/schema.rs`.

- [ ] **Step 4: Write a `lib.rs` for each of the three crates**

```rust
// crates/brain-search/src/lib.rs
pub mod schema;
pub mod scanner;
pub mod search_tool;

pub use search_tool::{BrainSearchParams, SearchHandler};

// crates/brain-graph/src/lib.rs
pub mod builder;
pub mod graph_tool;
pub mod model;
pub mod queries;
pub mod schema;
pub mod traversal;
pub mod watcher;

pub use graph_tool::GraphHandler;
pub use model::ProjectCodeGraph;

// crates/brain-engine/src/lib.rs
pub mod actor;
pub mod llama_validator;
pub mod prompt;

pub use actor::{ValidationClient, ValidationJob};
pub use llama_validator::LlamaValidator;
pub use prompt::PromptBuilder;
```

- [ ] **Step 5: Fix imports across all moved files**

Cross-crate replacements:
- `crate::error::` → `brain_kernel::` for `BrainError`/`Result`
- `crate::search::ast_chunker::AstSnippet` → `brain_kernel::AstSnippet`
- `crate::search::ast_chunker::SupportedLanguage` → `brain_kernel::SupportedLanguage`
- `crate::search::ast_chunker::AstChunker` → `brain_kernel::AstChunker`
- `crate::search::ranker::ScoredSnippet` → `brain_kernel::ScoredSnippet`
- `crate::search::ranker::LexicalRanker` → `brain_kernel::LexicalRanker`
- `crate::mcp::protocol::` → `brain_kernel::protocol::`
- `crate::engine::ValidationClient` → `brain_engine::ValidationClient`

Intra-crate replacements — the `graph::`, `engine::` and `search::` prefixes now refer to the crate root itself, so they must be dropped:
- In `brain-graph`: `crate::graph::model::*` → `crate::model::*`, same for `queries`, `traversal`, `builder`, `watcher`.
- In `brain-engine`: `crate::engine::prompt::PromptBuilder` → `crate::prompt::PromptBuilder`.
- In `brain-search`: `crate::search::scanner::LexicalScanner` → `crate::scanner::LexicalScanner`.

`graph_tool.rs` sits inside `brain-graph`, so its calls to `crate::graph::traversal::GraphTraversal` and `crate::graph::model::ProjectCodeGraph` become `crate::traversal::GraphTraversal` / `crate::model::ProjectCodeGraph`. It also needs `brain_engine` **not** — `GraphHandler` has no engine dependency.

`scanner.rs` uses the bare `regex::escape` path with no `use regex` line; add no import, it works as-is.

- [ ] **Step 6: Verify each crate compiles**

```bash
cargo build -p brain-kernel -p brain-search -p brain-graph -p brain-engine 2>&1 | grep -E "^error|warning: unused" | head -20
```

Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: move search, graph and engine code into their own crates"
```

---

### Task 4: Add the plugin registry to the kernel

**Files:**
- Create: `crates/brain-kernel/src/plugin.rs`
- Modify: `crates/brain-kernel/src/lib.rs`
- Modify: `crates/brain-kernel/Cargo.toml` (add `[dev-dependencies]` `tokio = { workspace = true }` — only `plugin.rs` tests need a runtime)
- Test: inline `#[cfg(test)] mod tests` in `plugin.rs`

**Interfaces:**
- Consumes: `brain_kernel::protocol::{Tool, CallToolResult}`.
- Produces:
  - `pub trait ToolPlugin: Send + Sync { fn name(&self) -> &'static str; fn schema(&self) -> Tool; fn call(&self, args: serde_json::Value) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>; }`
  - `pub struct Registry`
  - `Registry::new() -> Self`
  - `Registry::register(&mut self, plugin: Arc<dyn ToolPlugin>) -> Result<()>` — returns `BrainError::Config` if the name is already registered.
  - `Registry::list_tools(&self) -> Vec<Tool>`
  - `Registry::call_tool(&self, name: &str, args: serde_json::Value) -> impl Future<Output = CallToolResult>`
  - `impl ToolDispatcher for Registry`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn empty_registry_lists_no_tools() {
    let reg = Registry::new();
    assert!(reg.list_tools().is_empty());
}

#[test]
fn duplicate_name_is_rejected_at_registration() {
    let mut reg = Registry::new();
    reg.register(Arc::new(FakePlugin("dup"))).unwrap();
    let err = reg.register(Arc::new(FakePlugin("dup"))).unwrap_err();
    assert!(matches!(err, BrainError::Config(_)));
}

#[tokio::test]
async fn unknown_tool_returns_error_result() {
    let reg = Registry::new();
    let out = reg.call_tool("nope", serde_json::json!({})).await;
    assert_eq!(out.is_error, Some(true));
}
```

`#[tokio::test]` is why `brain-kernel` needs the tokio dev-dependency. `FakePlugin` is a local struct in the test module:

```rust
struct FakePlugin(&'static str);

impl ToolPlugin for FakePlugin {
    fn name(&self) -> &'static str { self.0 }
    fn schema(&self) -> Tool {
        Tool { name: self.0.to_string(), description: String::new(),
               input_schema: serde_json::json!({"type": "object"}) }
    }
    fn call(&self, _args: serde_json::Value)
        -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>
    {
        Box::pin(async { CallToolResult::text("ok") })
    }
}
```

`CallToolResult::is_error` is `Option<bool>`, not `bool` — `None` on success, `Some(true)` on error. Compare with `assert_eq!(…, Some(true))`.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brain-kernel plugin 2>&1 | grep -E "error\[|cannot find"
```

Expected: FAIL — `Registry` not defined.

- [ ] **Step 3: Implement `plugin.rs`**

`Registry` holds `HashMap<&'static str, Arc<dyn ToolPlugin>>`. `register` inserts and returns `BrainError::Config` on an occupied key. `list_tools` maps plugins to schemas **preserving registration order** (keep a `Vec<&'static str>` of insertion order alongside the map, so tool listing is stable across runs). `call_tool` looks up and awaits; a miss returns `CallToolResult::error(format!("Unknown tool: {name}"))`.

Also move `ToolDispatcher` from `src/mcp/server.rs` into this module and implement it for `Registry`. Move `EmptyDispatcher` too — it has no dependencies beyond `CallToolResult`, and `tests/mcp_protocol_test.rs` imports it. `plugin.rs` therefore needs no `tokio` for the trait itself: `ToolDispatcher::call_tool` and `ToolPlugin::call` both return `Pin<Box<dyn Future …>>` without requiring a runtime.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p brain-kernel plugin 2>&1 | grep -E "^test |test result"
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/brain-kernel/src/plugin.rs crates/brain-kernel/src/lib.rs
git commit -m "feat(kernel): add ToolPlugin trait and Registry with fail-fast name checks"
```

---

### Task 5: Make the tools implement `ToolPlugin`

**Files:**
- Modify: `crates/brain-search/src/search_tool.rs`
- Modify: `crates/brain-graph/src/graph_tool.rs`
- Test: inline unit tests asserting `name()` and `schema().name`

**Interfaces:**
- Consumes: `brain_kernel::plugin::ToolPlugin` from Task 4.
- Produces: `SearchHandler` implements `ToolPlugin` (name `"brain_search"`); `GraphHandler` implements `ToolPlugin` **twice is not possible** — so create `GraphContextTool` and `GraphImpactTool` wrappers, each holding an `Arc<GraphHandler>` and delegating to the respective method. Names `"brain_context_graph"` and `"brain_impact_check"`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn search_plugin_announces_its_name_and_schema() {
    let p = SearchHandler::for_test();          // fixture helper, no model
    assert_eq!(p.name(), "brain_search");
    assert_eq!(p.schema().name, "brain_search");
}
```

Repeat the shape in the graph crate for `GraphContextTool` → `"brain_context_graph"` and `GraphImpactTool` → `"brain_impact_check"`.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p brain-search -p brain-graph 2>&1 | grep -E "error\[|not satisfied"
```

Expected: FAIL.

- [ ] **Step 3: Implement the trait impls**

`SearchHandler::call` delegates to the existing `handle_brain_search`. Add the test fixture as a plain constructor — no mock needed, because `ValidationClient::new(None, 0)` already builds the lexical fallback:

```rust
pub fn for_test() -> Self {
    Self::new(".", ValidationClient::new(None, 0), 5, 0.65)
}
```

Graph fixtures build from `ProjectCodeGraph::new()`:

```rust
fn for_test_graph() -> GraphHandler {
    GraphHandler::new(".", Arc::new(tokio::sync::RwLock::new(ProjectCodeGraph::new())))
}
```

- [ ] **Step 4: Run to verify pass**

```bash
cargo test -p brain-search -p brain-graph 2>&1 | grep -E "^test |test result"
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: expose search and graph tools as ToolPlugin implementations"
```

---

### Task 6: Rewire the binary crate and preserve module paths

**Files:**
- Create: `crates/brain-tools/src/lib.rs`
- Create: `crates/brain-tools/src/main.rs`
- Move: `src/main.rs` → `crates/brain-tools/src/main.rs`
- Delete: now-empty `src/` tree, root `src/lib.rs`

**Interfaces:**
- Consumes: all four lower crates.
- Produces: `brain_tools::{config, engine, error, graph, mcp, search, ast_chunker}` re-export paths that satisfy every existing `use brain_tools::...` in `tests/`.

- [ ] **Step 1: Write `crates/brain-tools/src/lib.rs` with compatibility re-exports**

```rust
pub use brain_kernel::error;
pub use brain_kernel::{BrainError, Config, Result};

pub mod search {
    pub use brain_kernel::{ast as ast_chunker, ranking as ranker};
    pub use brain_search::scanner;
}
pub mod graph {
    pub use brain_graph::{builder, model, queries, traversal, watcher};
}
pub mod engine {
    pub use brain_engine::{actor, llama_validator, prompt};
}
pub mod mcp {
    pub use brain_kernel::protocol;
    pub mod handlers {
        pub use brain_graph::graph_tool::{GraphHandler, GraphImpactTool, GraphContextTool};
        pub use brain_search::search_tool::SearchHandler;
    }
    pub mod schemas {
        pub use brain_graph::schema::{brain_context_graph_tool, brain_impact_check_tool};
        pub use brain_search::schema::brain_search_tool;
    }
    pub mod server {
        pub use brain_kernel::plugin::{EmptyDispatcher, ToolDispatcher};
        pub use crate::mcp_server::{BrainToolDispatcher, McpServer};
    }
}
```

Note: `brain_tools::search::ast_chunker::AstSnippet` requires `ast` to expose that item — it does.

- [ ] **Step 2: Create `crates/brain-tools/src/mcp_server.rs`**

Move the `run_loop` I/O loop and `McpServer` out of the old `src/mcp/server.rs` verbatim. Keep `EmptyDispatcher` in the kernel (Task 4) and re-export it.

`tests/brain_search_integration_test.rs` calls `BrainToolDispatcher::new(search_handler, graph_handler)` with concrete handlers, so keep that constructor as a legacy adapter: it builds a `Registry` from `SearchHandler`, `GraphContextTool` and `GraphImpactTool` and delegates both trait methods to it. The three names are distinct by construction, so the `register` results are safe to `expect`.

- [ ] **Step 3: Verify the existing tests compile and pass unchanged**

```bash
cargo test 2>&1 | grep -E "^test result|^error" | head -30
```

Expected: all suites pass, **no edits to `tests/*.rs`**. This is the compatibility gate. If a path is missing, add the re-export rather than editing the test.

- [ ] **Step 4: Rewrite `main.rs` to mount the registry**

Replace the three-handler wiring with:

```rust
let mut registry = Registry::new();
registry.register(Arc::new(SearchHandler::new(&config.root_dir, validator, config.default_max_results, config.default_min_confidence)))?;
registry.register(Arc::new(GraphContextTool::new(graph_handler.clone())))?;
registry.register(Arc::new(GraphImpactTool::new(graph_handler)))?;
let server = McpServer::new(Arc::new(registry));
```

Keep the existing startup order: logging → model resolution → search handler → graph build → watcher → registry → `run_loop`.

- [ ] **Step 5: Verify the binary starts and lists three tools**

```bash
printf '%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
 | cargo run -p brain-tools -- --root-dir . 2>/dev/null | tail -1 \
 | grep -o '"name":"brain_[a-z_]*"' | sort -u
```

Expected: exactly three names — `brain_context_graph`, `brain_impact_check`, `brain_search`.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(bin): mount tools through the registry and preserve legacy module paths"
```

---

### Task 7: Delete the old tree and verify the workspace is clean

**Files:**
- Delete: root `src/` (all remaining files, now empty or duplicated)
- Modify: root `Cargo.toml` if any stale `[package]` section remains

**Interfaces:**
- Consumes: Tasks 1–6.
- Produces: a workspace where `src/` no longer exists at root and `cargo build` is warning-free.

- [ ] **Step 1: Confirm no source remains outside `crates/`**

```bash
test ! -d src && echo "root src/ gone"
find . -name "*.rs" -not -path "./crates/*" -not -path "./target/*" | head
```

Expected: `root src/ gone`, and the `find` prints nothing — `tests/` moved to `crates/brain-tools/tests/` in Task 1.

- [ ] **Step 2: Check for warnings**

```bash
cargo build --all-targets 2>&1 | grep -c "^warning"
```

Expected: `0`, or only pre-existing warnings unrelated to the move. Fix any `unused import` the move introduced.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "refactor: remove the legacy root src tree"
```

---

### Task 8: Verify both build profiles and the real scenario

**Files:** none (verification only).

**Interfaces:**
- Consumes: the finished workspace.

- [ ] **Step 1: Full test suite, default profile**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED"
```

Expected: every suite `ok`, zero failures.

- [ ] **Step 2: GPU profile still builds**

```bash
cargo build --release --features cuda 2>&1 | tail -5
```

Expected: `Finished`. If this fails while the default build passed, the feature propagation in Task 1 step 3 is wrong — fix it there rather than here.

- [ ] **Step 3: End-to-end check in the real repository**

```bash
printf '%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"brain_search","arguments":{"natural_query":"quem reage ao fato Starved","keywords":["Starved","HungerSet"],"max_results":3}}}' \
 | timeout 200 env -C /home/desktop/RustroverProjects/dreamstrategygame \
     ./target/release/brain-tools \
     --model-path ./models/vllm-d3-edge-Q8_0.gguf --gpu-layers 33 2>/dev/null | tail -1
```

Expected: exit 0, a `### Resultados da Busca` block naming `Starved` or `HungerSet`, and no `GGML_ASSERT` in stderr.

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit -m "test: verify microkernel split across both build profiles"
```
