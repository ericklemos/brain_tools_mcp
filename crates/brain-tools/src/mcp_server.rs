use brain_kernel::plugin::ToolDispatcher;
use brain_kernel::protocol::*;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

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

/// Legacy two-handler wiring, kept so existing integrations keep compiling.
///
/// Builds a [`brain_kernel::Registry`] from the concrete handlers and
/// delegates to it, so there is one dispatch path in the process.
pub struct BrainToolDispatcher {
    registry: brain_kernel::Registry,
}

impl BrainToolDispatcher {
    pub fn new(
        search_handler: Arc<brain_search::SearchHandler>,
        graph_handler: Arc<brain_graph::GraphHandler>,
    ) -> Self {
        let mut registry = brain_kernel::Registry::new();
        // Distinct, constant names — a duplicate here is a programming error.
        registry
            .register(search_handler)
            .expect("brain_search registered once");
        registry
            .register(Arc::new(brain_graph::graph_tool::GraphContextTool::new(
                graph_handler.clone(),
            )))
            .expect("brain_context_graph registered once");
        registry
            .register(Arc::new(brain_graph::graph_tool::GraphImpactTool::new(
                graph_handler,
            )))
            .expect("brain_impact_check registered once");
        Self { registry }
    }
}

impl ToolDispatcher for BrainToolDispatcher {
    fn list_tools(&self) -> Vec<brain_kernel::protocol::Tool> {
        self.registry.list_tools()
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = brain_kernel::protocol::CallToolResult> + Send + '_,
        >,
    > {
        self.registry.call_tool(name, arguments)
    }
}
