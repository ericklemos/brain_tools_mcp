//! `initialize` must describe the tools this session exposes, not another
//! profile's: a read-only session taught to write spends a call discovering
//! that `brain_edit` is not there.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_kernel::protocol::{
    CallToolResult, InitializeResult, JsonRpcRequest, RequestId, Tool,
};
use brain_tools::mcp::server::{McpServer, ToolDispatcher};

/// Stands in for `--tools read`: the write tools are absent from the registry.
struct ReadOnlyProfile;

impl ToolDispatcher for ReadOnlyProfile {
    fn list_tools(&self) -> Vec<Tool> {
        ["brain_search", "brain_read", "brain_peek", "brain_map"]
            .into_iter()
            .map(|name| Tool {
                name: name.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object" }),
            })
            .collect()
    }

    fn call_tool(
        &self,
        _name: &str,
        _arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(async { CallToolResult::error("not used".to_string()) })
    }
}

#[tokio::test]
async fn initialize_teaches_only_the_tools_this_session_has() {
    let server = McpServer::new(Arc::new(ReadOnlyProfile));
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(1)),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        })),
    };

    let response = server.handle_request(request).await.expect("response");
    let result: InitializeResult = serde_json::from_value(response.result.unwrap()).unwrap();
    let text = result.instructions.unwrap_or_default();

    assert!(text.contains("brain_search"), "{text}");
    assert!(
        !text.contains("brain_edit"),
        "read-only session taught to write: {text}"
    );
    assert!(text.contains("--tools core"), "missing the way out: {text}");
}
