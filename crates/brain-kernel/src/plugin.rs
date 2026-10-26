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

/// The single seam between the MCP transport and the tools.
pub trait ToolDispatcher: Send + Sync {
    fn list_tools(&self) -> Vec<Tool>;
    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>>;
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
#[derive(Default)]
pub struct Registry {
    plugins: HashMap<&'static str, Arc<dyn ToolPlugin>>,
    order: Vec<&'static str>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
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
}

impl ToolDispatcher for Registry {
    fn list_tools(&self) -> Vec<Tool> {
        self.order
            .iter()
            .filter_map(|name| self.plugins.get(name))
            .map(|plugin| plugin.schema())
            .collect()
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        match self.plugins.get(name) {
            Some(plugin) => plugin.call(arguments),
            None => {
                let msg = format!("Unknown tool: {name}");
                Box::pin(async move { CallToolResult::error(msg) })
            }
        }
    }
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
}
