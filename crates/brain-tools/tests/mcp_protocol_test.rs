use brain_tools::mcp::protocol::*;
use brain_tools::mcp::server::{EmptyDispatcher, McpServer, ToolDispatcher};
use std::sync::Arc;

struct MockDispatcher;

impl ToolDispatcher for MockDispatcher {
    fn list_tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": { "type": "string" }
                }
            }),
        }]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallToolResult> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            if name == "test_tool" {
                let arg = arguments.get("arg").and_then(|v| v.as_str()).unwrap_or("none");
                CallToolResult::text(format!("Result: {arg}"))
            } else {
                CallToolResult::error("Unknown tool")
            }
        })
    }
}

#[tokio::test]
async fn test_mcp_initialize_and_tools_list() {
    let server = McpServer::new(Arc::new(MockDispatcher));

    // 1. Initialize
    let init_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(1)),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        })),
    };

    let resp = server.handle_request(init_req).await.expect("Expected response");
    assert_eq!(resp.id, Some(RequestId::Int(1)));
    let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.server_info.name, "brain-tools");
    assert!(result.capabilities.tools.is_some());

    // 2. Tools list
    let list_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::String("req-2".to_string())),
        method: "tools/list".to_string(),
        params: None,
    };

    let resp = server.handle_request(list_req).await.expect("Expected response");
    assert_eq!(resp.id, Some(RequestId::String("req-2".to_string())));
    let result: ListToolsResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].name, "test_tool");

    // 3. Tools call
    let call_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(3)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "test_tool",
            "arguments": { "arg": "brain_power" }
        })),
    };

    let resp = server.handle_request(call_req).await.expect("Expected response");
    let result: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.content[0].text, "Result: brain_power");
}

#[tokio::test]
async fn test_mcp_stream_run_loop() {
    let server = McpServer::new(Arc::new(EmptyDispatcher));

    let input = b"{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"ping\"}\n";
    let mut reader = &input[..];
    let mut output = Vec::new();

    server.run_loop(&mut reader, &mut output).await.unwrap();

    let output_str = String::from_utf8(output).unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
    assert_eq!(resp.id, Some(RequestId::Int(100)));
    assert_eq!(resp.result, Some(serde_json::json!({})));
}
