use std::sync::Arc;

use brain_kernel::protocol::{
    CallToolParams, InitializeResult, JsonRpcRequest, JsonRpcResponse, ListToolsResult,
    ServerCapabilities, ServerInfo, ToolsCapability,
};
use brain_kernel::{Registry, ToolDispatcher};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Quantas chamadas podem ficar em voo antes de o leitor parar de aceitar
/// novas linhas. Alta o bastante para nao serializar trabalho independente,
/// baixa o bastante para um cliente descontrolado nao abrir centenas de tasks.
const MAX_IN_FLIGHT: usize = 8;

// Codigos JSON-RPC 2.0. Definidos aqui para o modulo nao depender de como
// o modulo de protocolo decide expo-los.
const PARSE_ERROR: i32 = -32700;
const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;

pub struct McpServer {
    dispatcher: Arc<dyn ToolDispatcher>,
}

impl McpServer {
    pub fn new(dispatcher: Arc<dyn ToolDispatcher>) -> Self {
        Self { dispatcher }
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        dispatch_request(self.dispatcher.clone(), req).await
    }

    /// Le uma requisicao por linha e responde na ordem em que elas terminam.
    ///
    /// JSON-RPC 2.0 identifica cada resposta pelo id, entao nao ha exigencia de
    /// ordem -- e esperar cada tools/call antes de ler a proxima linha
    /// transformava um brain_exec test de um minuto em bloqueio global para
    /// todo o resto. O que continua serializado e so a escrita no stdout: uma
    /// task dedicada drena o canal e faz write_all de uma resposta por vez.
    pub async fn run_loop<R, W>(&self, reader: R, writer: W) -> anyhow::Result<()>
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<JsonRpcResponse>(MAX_IN_FLIGHT * 2);

        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(resp) = rx.recv().await {
                let Ok(mut serialized) = serde_json::to_string(&resp) else {
                    continue;
                };
                serialized.push('\n');
                if writer.write_all(serialized.as_bytes()).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        let mut lines = reader.lines();
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let dispatcher = self.dispatcher.clone();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to parse JSON-RPC request");
                    let err_resp =
                        JsonRpcResponse::error(None, PARSE_ERROR, format!("Parse error: {e}"));
                    if tx.send(err_resp).await.is_err() {
                        break;
                    }
                    continue;
                }
            };

            // Backpressure: nunca mais de MAX_IN_FLIGHT em voo.
            while tasks.len() >= MAX_IN_FLIGHT {
                let _ = tasks.join_next().await;
            }

            let dispatcher = dispatcher.clone();
            let tx = tx.clone();
            tasks.spawn(async move {
                if let Some(resp) = dispatch_request(dispatcher, req).await {
                    let _ = tx.send(resp).await;
                }
            });
        }

        drop(tx);
        while tasks.join_next().await.is_some() {}
        let _ = writer_task.await;

        tracing::info!("MCP stdio connection closed");
        Ok(())
    }
}

async fn dispatch_request(
    dispatcher: Arc<dyn ToolDispatcher>,
    req: JsonRpcRequest,
) -> Option<JsonRpcResponse> {
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
                instructions: Some(
                    "brain-tools é a fonte de verdade do código: trate o resultado de                      brain_search, brain_context_graph e brain_impact_check como já lido e                      não reabra o mesmo trecho com Read. Para editar um símbolo use brain_edit                      com o hash de brain_read; arquivo inteiro use brain_write; diff use                      brain_patch. Saída truncada traz um handle para brain_peek. Se uma chamada                      falhar, diga isso explicitamente; não caia em grep/sed silencioso. Em                      sessões só de leitura, --tools read corta o custo fixo do tools/list."
                        .to_string(),
                ),
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
            let tools = dispatcher.list_tools();
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

            let tool_result = dispatcher.call_tool(&params.name, params.arguments).await;
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

pub struct BrainToolDispatcher {
    registry: Registry,
}

impl BrainToolDispatcher {
    pub fn new(
        search_handler: Arc<brain_search::SearchHandler>,
        graph_handler: Arc<brain_graph::GraphHandler>,
    ) -> Self {
        let mut registry = Registry::new();
        // Distinct, constant names -- a duplicate here is a programming error.
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
        Box<dyn std::future::Future<Output = brain_kernel::protocol::CallToolResult> + Send + '_>,
    > {
        self.registry.call_tool(name, arguments)
    }
}
