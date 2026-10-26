use crate::mcp::protocol::*;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub trait ToolDispatcher: Send + Sync {
    fn list_tools(&self) -> Vec<Tool>;
    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallToolResult> + Send + '_>>;
}

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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallToolResult> + Send + '_>> {
        let msg = format!("Tool not found: {}", name);
        Box::pin(async move { CallToolResult::error(msg) })
    }
}

pub struct BrainToolDispatcher {
    search_handler: Arc<crate::mcp::handlers::SearchHandler>,
}

impl BrainToolDispatcher {
    pub fn new(search_handler: Arc<crate::mcp::handlers::SearchHandler>) -> Self {
        Self { search_handler }
    }
}

impl ToolDispatcher for BrainToolDispatcher {
    fn list_tools(&self) -> Vec<Tool> {
        vec![crate::mcp::schemas::brain_search_tool()]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallToolResult> + Send + '_>> {
        let name = name.to_string();
        let search_handler = self.search_handler.clone();
        Box::pin(async move {
            match name.as_str() {
                "brain_search" => search_handler.handle_brain_search(arguments).await,
                _ => CallToolResult::error(format!("Unknown tool: {name}")),
            }
        })
    }
}

pub struct McpServer {
    dispatcher: Arc<dyn ToolDispatcher>,
}

impl McpServer {
    pub fn new(dispatcher: Arc<dyn ToolDispatcher>) -> Self {
        Self { dispatcher }
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = req.id;
        let method = req.method.as_str();

        tracing::debug!(method = %method, "Processing MCP request");

        match method {
            "initialize" => {
                let result = InitializeResult {
                    protocol_version: "2024-11-05".to_string(),
                    capabilities: ServerCapabilities {
                        tools: Some(ToolsCapability {
                            list_changed: Some(false),
                        }),
                    },
                    server_info: ServerInfo {
                        name: "brain-tools".to_string(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                };
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::to_value(result).unwrap(),
                ))
            }
            "notifications/initialized" | "initialized" => {
                // Notifications do not receive a response unless an id was passed
                if id.is_some() {
                    Some(JsonRpcResponse::success(id, serde_json::json!({})))
                } else {
                    None
                }
            }
            "tools/list" => {
                let tools = self.dispatcher.list_tools();
                let result = ListToolsResult { tools };
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::to_value(result).unwrap(),
                ))
            }
            "tools/call" => {
                let params = match req.params {
                    Some(val) => match serde_json::from_value::<CallToolParams>(val) {
                        Ok(p) => p,
                        Err(e) => {
                            return Some(JsonRpcResponse::error(
                                id,
                                INVALID_PARAMS,
                                format!("Invalid tools/call params: {e}"),
                            ));
                        }
                    },
                    None => {
                        return Some(JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            "Missing parameters for tools/call",
                        ));
                    }
                };

                let tool_result = self.dispatcher.call_tool(&params.name, params.arguments).await;
                Some(JsonRpcResponse::success(
                    id,
                    serde_json::to_value(tool_result).unwrap(),
                ))
            }
            "ping" => Some(JsonRpcResponse::success(id, serde_json::json!({}))),
            _ => {
                tracing::warn!(method = %method, "Unknown method requested");
                Some(JsonRpcResponse::error(
                    id,
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                ))
            }
        }
    }

    pub async fn run_loop<R, W>(&self, reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: AsyncBufRead + Send + Unpin,
        W: AsyncWrite + Send + Unpin,
    {
        let mut lines = reader.lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to parse JSON-RPC request");
                    let err_resp = JsonRpcResponse::error(None, PARSE_ERROR, format!("Parse error: {e}"));
                    let mut serialized = serde_json::to_string(&err_resp)?;
                    serialized.push('\n');
                    writer.write_all(serialized.as_bytes()).await?;
                    writer.flush().await?;
                    continue;
                }
            };

            if let Some(resp) = self.handle_request(req).await {
                let mut serialized = serde_json::to_string(&resp)?;
                serialized.push('\n');
                writer.write_all(serialized.as_bytes()).await?;
                writer.flush().await?;
            }
        }

        tracing::info!("MCP stdio connection closed");
        Ok(())
    }
}
