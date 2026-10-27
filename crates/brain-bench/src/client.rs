//! Minimal MCP stdio client: enough to initialize, list tools and call them.
//!
//! It speaks the same JSON-RPC the real agent does, so the measurement
//! includes what the agent would really receive — not an idealized copy.

use anyhow::{anyhow, bail, Context, Result};
use brain_kernel::protocol::{
    CallToolParams, CallToolResult, JsonRpcRequest, JsonRpcResponse, RequestId,
};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    timeout: Duration,
}

pub struct ToolCall {
    pub text: String,
    pub is_error: bool,
}

impl McpClient {
    pub async fn spawn(
        server: &Path,
        root: &Path,
        extra_args: &[String],
        timeout: Duration,
        verbose: bool,
    ) -> Result<Self> {
        let mut command = Command::new(server);
        command
            .arg("--root-dir")
            .arg(root)
            .arg("--log-level")
            .arg("warn")
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            });

        let mut child = command
            .spawn()
            .with_context(|| format!("falha ao iniciar o servidor {}", server.display()))?;
        let stdin = child.stdin.take().context("o servidor não abriu stdin")?;
        let stdout = child.stdout.take().context("o servidor não abriu stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            timeout,
        })
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "brain-bench", "version": env!("CARGO_PKG_VERSION") }
            })),
        )
        .await?;
        self.notify("notifications/initialized", None).await
    }

    /// Returns the raw `tools/list` result: the exact payload the client pays
    /// for, serialized the same way the server sends it.
    pub async fn list_tools(&mut self) -> Result<(String, usize)> {
        let result = self.request("tools/list", None).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|t| t.len())
            .unwrap_or(0);
        Ok((serde_json::to_string(&result)?, tools))
    }

    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolCall> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
        };
        let result = self
            .request("tools/call", Some(serde_json::to_value(&params)?))
            .await?;
        let result: CallToolResult =
            serde_json::from_value(result).context("resposta de tools/call fora do protocolo")?;
        let text = result
            .content
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolCall {
            text,
            is_error: result.is_error.unwrap_or(false),
        })
    }

    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>) -> Result<()> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = RequestId::Int(self.next_id);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(id.clone()),
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let mut buffer = String::new();
        let timeout = self.timeout;
        let read = self.stdout.read_line(&mut buffer);
        let read = tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| anyhow!("timeout de {}s esperando `{method}`", timeout.as_secs()))??;
        if read == 0 {
            bail!("o servidor fechou o stdio durante `{method}`");
        }

        let response: JsonRpcResponse = serde_json::from_str(buffer.trim())
            .with_context(|| format!("resposta JSON-RPC inválida para `{method}`: {buffer}"))?;
        if let Some(error) = response.error {
            bail!("`{method}` falhou: {} (code {})", error.message, error.code);
        }
        response
            .result
            .with_context(|| format!("`{method}` respondeu sem `result`"))
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Best effort: a benchmark that leaks a server process would keep the
        // indexer and the watcher alive after the run.
        let _ = self.child.start_kill();
    }
}
