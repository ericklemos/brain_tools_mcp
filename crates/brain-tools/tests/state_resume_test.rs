use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

async fn send(stdin: &mut tokio::process::ChildStdin, value: serde_json::Value) {
    let mut line = serde_json::to_string(&value).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

#[tokio::test]
async fn a_previous_session_reaches_the_next_initialize() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_brain-tools");
    let root = dir.path().to_str().unwrap();

    // First session: record a note and an open task.
    let mut child = Command::new(bin)
        .args(["--root-dir", root])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first daemon");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
        }),
    )
    .await;
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    for (id, arguments) in [
        (
            2,
            serde_json::json!({ "action": "note", "text": "lembrar do refresh token" }),
        ),
        (
            3,
            serde_json::json!({ "action": "task", "text": "terminar refresh", "status": "open" }),
        ),
    ] {
        send(
            &mut stdin,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": "brain_state", "arguments": arguments }
            }),
        )
        .await;
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_ne!(
            response["result"]["isError"].as_bool(),
            Some(true),
            "{response}"
        );
    }

    drop(stdin);
    child.wait().await.unwrap();

    // Second session: initialize must carry the previous session's trail.
    let mut child = Command::new(bin)
        .args(["--root-dir", root])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn second daemon");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
        }),
    )
    .await;
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let instructions = response["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("lembrar do refresh token"),
        "{instructions}"
    );
    assert!(instructions.contains("terminar refresh"), "{instructions}");

    drop(stdin);
    child.wait().await.unwrap();
}
