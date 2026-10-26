use brain_tools::mcp::protocol::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn test_daemon_smoke_stdio_purity() {
    let bin_path = env!("CARGO_BIN_EXE_brain-tools");

    let mut child = Command::new(bin_path)
        .arg("--root-dir")
        .arg(".")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn brain-tools daemon binary");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    // 1. Send initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        }
    });

    let mut line = serde_json::to_string(&init_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.unwrap();

    let init_resp: JsonRpcResponse = serde_json::from_str(response_line.trim())
        .expect("Stdout output was not valid JSON-RPC");
    assert_eq!(init_resp.id, Some(RequestId::Int(1)));
    let init_res: InitializeResult = serde_json::from_value(init_resp.result.unwrap()).unwrap();
    assert_eq!(init_res.server_info.name, "brain-tools");

    // 2. Send tools/list
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });

    let mut line2 = serde_json::to_string(&list_req).unwrap();
    line2.push('\n');
    stdin.write_all(line2.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut response_line2 = String::new();
    reader.read_line(&mut response_line2).await.unwrap();

    let list_resp: JsonRpcResponse = serde_json::from_str(response_line2.trim())
        .expect("Stdout output was not valid JSON-RPC");
    assert_eq!(list_resp.id, Some(RequestId::Int(2)));
    let list_res: ListToolsResult = serde_json::from_value(list_resp.result.unwrap()).unwrap();
    assert_eq!(list_res.tools.len(), 3);

    // Close stdin to gracefully shut down the daemon loop
    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
