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
    // Pin the names, not just the count: a tool that silently fails to register
    // and one that registers twice both leave the count looking plausible.
    let names: Vec<&str> = list_res.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "brain_fetch",
            "brain_websearch",
            "brain_research",
            "brain_search",
            "brain_context_graph",
            "brain_impact_check",
            "brain_peek",
            "brain_exec",
            "brain_shell",
            "brain_read",
            "brain_edit",
            "brain_write",
            "brain_patch",
            "brain_map",
            "brain_assert",
            "brain_diff",
            "brain_why",
            "brain_ledger",
            "brain_trace",
        ]
    );

    // Close stdin to gracefully shut down the daemon loop
    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn a_tool_profile_removes_unlisted_tools_instead_of_hiding_them() {
    let bin_path = env!("CARGO_BIN_EXE_brain-tools");

    let mut child = Command::new(bin_path)
        .args(["--root-dir", ".", "--tools", "read"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn brain-tools daemon binary");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    let request = |method: &str, id: u64, params: serde_json::Value| {
        let mut line = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .unwrap();
        line.push('\n');
        line
    };

    // initialize is mandatory before tools/list in the daemon's state machine.
    stdin
        .write_all(request("initialize", 1, serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        })).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    stdin
        .write_all(request("tools/list", 2, serde_json::json!({})).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(line.trim()).unwrap();
    let list: ListToolsResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "brain_fetch",
            "brain_websearch",
            "brain_research",
            "brain_search",
            "brain_context_graph",
            "brain_impact_check",
            "brain_peek",
            "brain_read",
            "brain_map",
            "brain_assert",
            "brain_diff",
            "brain_why",
        ]
    );

    // A tool outside the profile must fail as unknown, not answer silently.
    stdin
        .write_all(request("tools/call", 3, serde_json::json!({
            "name": "brain_edit",
            "arguments": {}
        })).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(line.trim()).unwrap();
    let call: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(call.is_error, Some(true));
    let text = serde_json::to_string(&call.content).unwrap();
    assert!(text.contains("Unknown tool"));

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
