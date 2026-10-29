use brain_tools::engine::ValidationClient;
use brain_tools::graph::builder::GraphBuilder;
use brain_tools::mcp::handlers::{GraphHandler, SearchHandler};
use brain_tools::mcp::protocol::*;
use brain_tools::mcp::server::{BrainToolDispatcher, McpServer};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::RwLock;

#[tokio::test]
async fn test_brain_context_graph_and_impact_check() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let jwt_file = src.join("jwt.rs");
    fs::write(
        &jwt_file,
        r#"
pub fn verify_token(raw: &str) -> bool {
    !raw.is_empty()
}
"#,
    )
    .unwrap();

    let middleware_file = src.join("middleware.rs");
    fs::write(
        &middleware_file,
        r#"
pub fn auth_guard(token: &str) -> bool {
    verify_token(token)
}
"#,
    )
    .unwrap();

    let handler_file = src.join("handler.rs");
    fs::write(
        &handler_file,
        r#"
pub fn route_request(token: &str) -> bool {
    auth_guard(token)
}
"#,
    )
    .unwrap();

    let builder = Arc::new(GraphBuilder::new().expect("Builder init failed"));
    let code_graph = Arc::new(RwLock::new(
        builder.build_graph(root).expect("Graph build failed"),
    ));

    let validator = ValidationClient::new();
    let search_handler = Arc::new(SearchHandler::new(root, validator, 5, 0.5));
    let graph_handler = Arc::new(GraphHandler::new(root, code_graph));
    let dispatcher = Arc::new(BrainToolDispatcher::new(search_handler, graph_handler));
    let server = McpServer::new(dispatcher);

    // 1. Check tools/list has 3 tools
    let list_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(1)),
        method: "tools/list".to_string(),
        params: None,
    };
    let resp = server.handle_request(list_req).await.unwrap();
    let list_res: ListToolsResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(list_res.tools.len(), 3);
    let tool_names: Vec<String> = list_res.tools.into_iter().map(|t| t.name).collect();
    assert!(tool_names.contains(&"brain_search".to_string()));
    assert!(tool_names.contains(&"brain_context_graph".to_string()));
    assert!(tool_names.contains(&"brain_impact_check".to_string()));

    // 2. Call brain_context_graph for verify_token
    let graph_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(2)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "brain_context_graph",
            "arguments": {
                "target_symbol_or_file": "verify_token",
                "depth": 2
            }
        })),
    };

    let resp = server.handle_request(graph_req).await.unwrap();
    let res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(res.is_error, None);
    let text = &res.content[0].text;
    assert!(
        text.contains("verify_token"),
        "Expected verify_token in output: {text}"
    );
    assert!(
        text.contains("auth_guard"),
        "Expected caller auth_guard in output: {text}"
    );

    // 3. Call brain_impact_check for verify_token
    let impact_req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Int(3)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "brain_impact_check",
            "arguments": {
                "symbol_name": "verify_token"
            }
        })),
    };

    let resp = server.handle_request(impact_req).await.unwrap();
    let res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(res.is_error, None);
    let impact_text = &res.content[0].text;
    assert!(impact_text.contains("Relatório de Análise de Impacto: `verify_token`"));
    assert!(
        impact_text.contains("auth_guard"),
        "Expected direct impact: {impact_text}"
    );
    assert!(
        impact_text.contains("route_request"),
        "Expected transitive impact: {impact_text}"
    );
}
