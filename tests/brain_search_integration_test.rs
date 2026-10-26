use brain_tools::engine::ValidationClient;
use brain_tools::mcp::handlers::SearchHandler;
use brain_tools::mcp::protocol::*;
use brain_tools::mcp::server::{BrainToolDispatcher, McpServer};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn test_brain_search_end_to_end_integration() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let rs_file = src.join("auth.rs");
    fs::write(
        &rs_file,
        r#"
/// Verifies JWT Bearer tokens
pub fn verify_jwt_token(raw_token: &str) -> bool {
    if raw_token.is_empty() {
        return false;
    }
    raw_token.starts_with("Bearer ")
}
"#,
    )
    .unwrap();

    let kt_file = src.join("BillingService.kt");
    fs::write(
        &kt_file,
        r#"
package com.example.billing

class BillingService {
    fun process_invoice(amount: Double): Boolean {
        return amount > 0.0
    }
}
"#,
    )
    .unwrap();

    // Initialize with fallback mode (no GGUF model path)
    let validator = ValidationClient::new(None, 0);
    let search_handler = Arc::new(SearchHandler::new(root, validator, 5, 0.5));
    let dispatcher = Arc::new(BrainToolDispatcher::new(search_handler));
    let server = McpServer::new(dispatcher);

    // 1. Verify brain_search is listed in tools/list
    let list_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(1)),
        method: "tools/list".to_string(),
        params: None,
    };

    let resp = server.handle_request(list_req).await.expect("Expected tools/list response");
    let list_res: ListToolsResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(list_res.tools.len(), 1);
    assert_eq!(list_res.tools[0].name, "brain_search");

    // 2. Call brain_search for Rust function
    let call_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(2)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "brain_search",
            "arguments": {
                "natural_query": "jwt token verification",
                "keywords": ["verify_jwt_token"]
            }
        })),
    };

    let resp = server.handle_request(call_req).await.expect("Expected tools/call response");
    let call_res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(call_res.is_error, None);
    let text = &call_res.content[0].text;
    assert!(text.contains("verify_jwt_token"), "Expected symbol name in output: {text}");
    assert!(text.contains("pub fn verify_jwt_token"), "Expected signature in output: {text}");
    assert!(text.contains("src/auth.rs:3-8"), "Expected line location in output: {text}");

    // 3. Call brain_search for Kotlin function
    let call_kt = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(3)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "brain_search",
            "arguments": {
                "natural_query": "invoice billing payment",
                "keywords": ["process_invoice"]
            }
        })),
    };

    let resp_kt = server.handle_request(call_kt).await.expect("Expected tools/call response");
    let call_res_kt: CallToolResult = serde_json::from_value(resp_kt.result.unwrap()).unwrap();
    assert_eq!(call_res_kt.is_error, None);
    let text_kt = &call_res_kt.content[0].text;
    assert!(text_kt.contains("process_invoice"), "Expected symbol name in output: {text_kt}");
    assert!(text_kt.contains("fun process_invoice"), "Expected signature in output: {text_kt}");
}
