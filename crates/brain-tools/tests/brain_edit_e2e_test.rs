use brain_tools::mcp::protocol::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

async fn call(
    stdin: &mut tokio::process::ChildStdin,
    reader: &mut BufReader<tokio::process::ChildStdout>,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> JsonRpcResponse {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&request).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.unwrap();
    serde_json::from_str(response_line.trim()).expect("stdout was not valid JSON-RPC")
}

fn tool_text(response: JsonRpcResponse) -> CallToolResult {
    serde_json::from_value(response.result.expect("missing result")).expect("not a tool result")
}

fn hash_from(text: &str) -> String {
    let marker = "blake3 `";
    let start = text.find(marker).expect("no blake3 in response") + marker.len();
    text[start..start + 12].to_string()
}

#[tokio::test]
async fn read_then_edit_then_read_again_shows_the_new_body_and_hash() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("auth.rs");
    std::fs::write(
        &file,
        "pub fn verify(raw: &str) -> bool {\n    !raw.is_empty()\n}\n\npub fn logout() {\n}\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_brain-tools"))
        .arg("--root-dir")
        .arg(dir.path())
        .arg("--exec-disabled")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn daemon");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let _ = call(
        &mut stdin,
        &mut reader,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        }),
    )
    .await;

    let read = tool_text(
        call(
            &mut stdin,
            &mut reader,
            2,
            "tools/call",
            serde_json::json!({
                "name": "brain_read",
                "arguments": { "path": "auth.rs", "symbol": "verify" }
            }),
        )
        .await,
    );
    let old_hash = hash_from(&read.content[0].text);

    let edit = tool_text(
        call(
            &mut stdin,
            &mut reader,
            3,
            "tools/call",
            serde_json::json!({
                "name": "brain_edit",
                "arguments": {
                    "path": "auth.rs",
                    "symbol": "verify",
                    "expected_hash": old_hash,
                    "new_body": "{\n    raw.len() > 3\n}"
                }
            }),
        )
        .await,
    );
    assert_eq!(edit.is_error, None, "{}", edit.content[0].text);

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains("raw.len() > 3"), "{on_disk}");
    assert!(!on_disk.contains("!raw.is_empty()"), "{on_disk}");

    let read_again = tool_text(
        call(
            &mut stdin,
            &mut reader,
            4,
            "tools/call",
            serde_json::json!({
                "name": "brain_read",
                "arguments": { "path": "auth.rs", "symbol": "verify" }
            }),
        )
        .await,
    );
    let new_text = &read_again.content[0].text;
    assert!(new_text.contains("raw.len() > 3"), "{new_text}");
    assert_ne!(hash_from(new_text), old_hash);

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
