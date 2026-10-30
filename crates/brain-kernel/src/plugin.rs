use crate::error::BrainError;
use crate::protocol::{CallToolResult, Tool};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A tool exposed over MCP, registered into the kernel at startup.
///
/// `name()` is `&'static str` so registration-order maps can key on it without
/// allocation, and so two plugins claiming the same name is a compile-time
/// visible mistake at the call site rather than a runtime surprise.
pub trait ToolPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> Tool;
    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>;
}

/// Lets an `Arc<Plugin>` be registered directly, so callers holding an `Arc`
/// for another reason (a handler shared between plugins) need no adapter.
impl<T: ToolPlugin + ?Sized> ToolPlugin for Arc<T> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn schema(&self) -> Tool {
        (**self).schema()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        (**self).call(args)
    }
}

/// The single seam between the MCP transport and the tools.
pub trait ToolDispatcher: Send + Sync {
    fn list_tools(&self) -> Vec<Tool>;
    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>;
    /// Extra text appended to the MCP initialize instructions.
    fn instructions(&self) -> Option<String> {
        None
    }
}

/// A dispatcher with no tools, for protocol-level tests.
#[derive(Default)]
pub struct EmptyDispatcher;

impl ToolDispatcher for EmptyDispatcher {
    fn list_tools(&self) -> Vec<Tool> {
        vec![]
    }

    fn call_tool(
        &self,
        name: &str,
        _arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let msg = format!("Tool not found: {}", name);
        Box::pin(async move { CallToolResult::error(msg) })
    }
}

/// Holds the registered plugins and answers `tools/list` / `tools/call`.
///
/// Insertion order is preserved in `order` so `tools/list` is stable across
/// runs; a `HashMap` alone would reorder it.
fn take_preinstalled() -> Vec<Arc<dyn ToolPlugin>> {
    std::mem::take(&mut *PREINSTALLED.lock().unwrap())
}

/// A tool that needs something the registry does not have yet (the handle
/// store is built before the registry in main). It waits here, and every
/// registry created afterwards adopts it, so ordering, profile filtering and
/// name uniqueness behave exactly as for inline plugins.
pub fn install(plugin: Arc<dyn ToolPlugin>) {
    PREINSTALLED.lock().unwrap().push(plugin);
}

static PREINSTALLED: std::sync::Mutex<Vec<Arc<dyn ToolPlugin>>> = std::sync::Mutex::new(Vec::new());

#[derive(Default)]
pub struct Registry {
    plugins: HashMap<&'static str, Arc<dyn ToolPlugin>>,
    order: Vec<&'static str>,
    state: Option<Arc<crate::state::StateStore>>,
}

impl Registry {
    pub fn with_state(mut self, state: Arc<crate::state::StateStore>) -> Self {
        self.state = Some(state);
        self
    }

    pub fn new() -> Self {
        let mut registry = Self::default();
        for plugin in take_preinstalled() {
            registry
                .register(plugin)
                .expect("pre-installed tool names are unique");
        }
        registry
    }

    /// Registers a plugin, failing fast on a duplicate name.
    ///
    /// Two plugins answering the same name means one of them is silently
    /// unreachable for the life of the process, so this is an error rather
    /// than a replace.
    pub fn register(&mut self, plugin: Arc<dyn ToolPlugin>) -> crate::Result<()> {
        let name = plugin.name();
        if self.plugins.contains_key(name) {
            return Err(BrainError::Config(format!(
                "Tool '{name}' is already registered"
            )));
        }
        self.plugins.insert(name, plugin);
        self.order.push(name);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Restricts the exposed set to `allowed`, preserving registration order.
    ///
    /// Restricted tools are removed rather than hidden in `list_tools`: a
    /// profile that still answers `tools/call` would not save the schema it was
    /// meant to save, and a hidden tool is a tool the client did not negotiate.
    pub fn retain(&mut self, allowed: &[&'static str]) {
        let allowed: std::collections::HashSet<&'static str> = allowed.iter().copied().collect();
        self.order.retain(|name| allowed.contains(name));
        self.plugins.retain(|name, _| allowed.contains(name));
    }
}

impl ToolDispatcher for Registry {
    fn list_tools(&self) -> Vec<Tool> {
        self.order
            .iter()
            .filter_map(|name| self.plugins.get(name))
            .map(|plugin| plugin.schema())
            .collect()
    }

    fn instructions(&self) -> Option<String> {
        self.state.as_ref().and_then(|state| state.resume())
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        match self.plugins.get(name) {
            Some(plugin) => {
                let state = self.state.clone();
                let summary = state
                    .as_ref()
                    .filter(|_| name != "brain_state")
                    .map(|_| CallSummary::new(name, &arguments));
                let started = std::time::Instant::now();
                let tool_name = name.to_string();
                let future = plugin.call(arguments);
                Box::pin(async move {
                    let result = future.await;
                    if let (Some(state), Some(summary)) = (state, summary) {
                        let ok = tool_ok(&tool_name, &result);
                        let bytes = result.content.iter().map(|item| item.text.len()).sum();
                        state.record_tool(crate::state::ToolRecord {
                            tool: &summary.tool,
                            target: summary.target.as_deref(),
                            path: summary.path.as_deref(),
                            symbol: summary.symbol.as_deref(),
                            ok,
                            ms: started.elapsed().as_millis() as u64,
                            bytes,
                        });
                    }
                    result
                })
            }
            None => {
                let msg = format!("Unknown tool: {name}");
                Box::pin(async move { CallToolResult::error(msg) })
            }
        }
    }
}

fn tool_ok(name: &str, result: &CallToolResult) -> bool {
    if result.is_error == Some(true) {
        return false;
    }
    if name != "brain_exec" {
        return true;
    }
    let headline = result
        .content
        .first()
        .map(|item| item.text.lines().next().unwrap_or(""))
        .unwrap_or("");
    !(headline.contains("saiu com código")
        || headline.contains("tempo esgotado")
        || headline.contains("falhou"))
}

struct CallSummary {
    tool: String,
    target: Option<String>,
    path: Option<String>,
    symbol: Option<String>,
}

impl CallSummary {
    fn new(tool: &str, arguments: &serde_json::Value) -> Self {
        let path = arg_str(arguments, "path");
        let symbol = arg_str(arguments, "symbol");
        let target = path
            .clone()
            .or_else(|| symbol.clone())
            .or_else(|| arg_str(arguments, "target_symbol_or_file"))
            .or_else(|| arg_str(arguments, "symbol_name"))
            .or_else(|| arg_str(arguments, "natural_query"))
            .or_else(|| arg_str(arguments, "query"))
            .or_else(|| arg_str(arguments, "kind"))
            .or_else(|| arg_str(arguments, "url"))
            .or_else(|| keywords(arguments))
            .map(|target| target.chars().take(160).collect());
        Self {
            tool: tool.to_string(),
            target,
            path,
            symbol,
        }
    }
}

fn arg_str(arguments: &serde_json::Value, key: &str) -> Option<String> {
    let value = arguments.get(key)?.as_str()?;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn keywords(arguments: &serde_json::Value) -> Option<String> {
    let values = arguments.get("keywords")?.as_array()?;
    let joined = values
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePlugin(&'static str);

    impl ToolPlugin for FakePlugin {
        fn name(&self) -> &'static str {
            self.0
        }

        fn schema(&self) -> Tool {
            Tool {
                name: self.0.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        fn call(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
            Box::pin(async { CallToolResult::text("ok") })
        }
    }

    #[test]
    fn empty_registry_lists_no_tools() {
        let reg = Registry::new();
        assert!(reg.list_tools().is_empty());
    }

    #[test]
    fn retain_keeps_only_the_allowed_tools_in_registration_order() {
        let mut reg = Registry::new();
        for name in ["a", "b", "c"] {
            reg.register(Arc::new(FakePlugin(name))).unwrap();
        }

        reg.retain(&["c", "a"]);

        let names: Vec<String> = reg.list_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn duplicate_name_is_rejected_at_registration() {
        let mut reg = Registry::new();
        reg.register(Arc::new(FakePlugin("dup"))).unwrap();
        let err = reg.register(Arc::new(FakePlugin("dup"))).unwrap_err();
        assert!(matches!(err, BrainError::Config(_)));
    }

    #[test]
    fn list_tools_preserves_registration_order() {
        let mut reg = Registry::new();
        reg.register(Arc::new(FakePlugin("b"))).unwrap();
        reg.register(Arc::new(FakePlugin("a"))).unwrap();
        reg.register(Arc::new(FakePlugin("c"))).unwrap();

        let names: Vec<String> = reg.list_tools().iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["b", "a", "c"]);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let reg = Registry::new();
        let out = reg.call_tool("nope", serde_json::json!({})).await;
        assert_eq!(out.is_error, Some(true));
    }
    #[test]
    fn exec_failure_headline_is_not_ok() {
        let result = CallToolResult {
            content: vec![crate::protocol::ToolContent {
                content_type: "text".to_string(),
                text: "### `test` · 1 falharam, 5 passaram · saiu com código 101 · 1.0s\n"
                    .to_string(),
            }],
            is_error: None,
        };
        assert!(!tool_ok("brain_exec", &result));
    }

    #[test]
    fn exec_success_headline_is_ok() {
        let result = CallToolResult {
            content: vec![crate::protocol::ToolContent {
                content_type: "text".to_string(),
                text: "### `test` · 0 falharam, 5 passaram · ok · 0.4s\n".to_string(),
            }],
            is_error: None,
        };
        assert!(tool_ok("brain_exec", &result));
    }
}
