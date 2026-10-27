use brain_tools::mcp::protocol::*;
use std::fs;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// `--root-dir` defaults to `.`; a tool that canonicalizes the file path but
/// not the root prints an absolute path on every line. That was a real token
/// leak in `brain_read` (42% of a large outline), so this locks it down at the
/// binary level, where the root is resolved.
#[tokio::test]
async fn read_paths_are_relative_to_a_dot_root() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();

    let bin_path = env!("CARGO_BIN_EXE_brain-tools");
    let mut child = Command::new(bin_path)
        .current_dir(dir.path())
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

    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    });
    let mut line = serde_json::to_string(&init).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.unwrap();

    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "brain_read", "arguments": { "path": "a.rs" } }
    });
    let mut line = serde_json::to_string(&call).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(response_line.trim()).expect("valid JSON-RPC");
    let res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let text = &res.content[0].text;

    assert!(text.contains("### `a.rs`"), "relative header expected: {text}");
    let absolute = dir.path().to_string_lossy().to_string();
    assert!(
        !text.contains(&absolute),
        "absolute path leaked into the response: {text}"
    );

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
