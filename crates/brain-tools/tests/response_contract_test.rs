use brain_tools::mcp::protocol::*;
use std::fs;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// End-to-end proof of the response contract on the real binary: a search that
/// does not fit its budget must announce the truncation and leave the rest
/// recoverable through `brain_peek`.
#[tokio::test]
async fn truncated_search_is_recoverable_through_brain_peek() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    for name in ["alpha_handler", "beta_handler"] {
        let body = format!(
            "pub fn {name}(input: &str) -> String {{\n{}\n    input.to_string()\n}}\n",
            "    let _ = input.len();\n".repeat(40)
        );
        fs::write(src.join(format!("{name}.rs")), body).unwrap();
    }

    let bin_path = env!("CARGO_BIN_EXE_brain-tools");
    let mut child = Command::new(bin_path)
        .current_dir(dir.path())
        .arg("--root-dir")
        .arg(dir.path())
        .arg("--default-budget-tokens")
        .arg("150")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn brain-tools daemon binary");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    let call = |id: i32, name: &str, args: serde_json::Value| {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        line
    };

    // 1. A search whose two snippets cannot both fit 120 tokens.
    let search_req = call(
        1,
        "brain_search",
        serde_json::json!({
            "natural_query": "handler",
            "keywords": ["handler"],
            "max_results": 10
        }),
    );
    stdin.write_all(search_req.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(line.trim()).expect("valid JSON-RPC");
    let res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let search_text = &res.content[0].text;

    assert!(
        search_text.contains("Resposta truncada"),
        "expected a truncation envelope: {search_text}"
    );

    let handle = search_text
        .split("handle: `")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .expect("handle in envelope")
        .to_string();

    // 2. The handle resolves and returns the part the envelope left out.
    let peek_req = call(
        2,
        "brain_peek",
        serde_json::json!({ "handle": handle, "budget_tokens": 4000 }),
    );
    stdin.write_all(peek_req.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(line.trim()).expect("valid JSON-RPC");
    let peek_res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        peek_res.is_error, None,
        "peek failed: {}",
        peek_res.content[0].text
    );

    let peek_text = &peek_res.content[0].text;
    assert!(
        peek_text.contains("handler"),
        "recovered body should hold the omitted snippet: {peek_text}"
    );
    assert!(
        peek_text.contains("Restante: 0 caracteres"),
        "a 4000-token page should cover the whole body: {peek_text}"
    );

    // 3. An unknown handle is a categorical error, never an empty answer.
    let bogus_req = call(
        3,
        "brain_peek",
        serde_json::json!({ "handle": "h_deadbeef" }),
    );
    stdin.write_all(bogus_req.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_str(line.trim()).expect("valid JSON-RPC");
    let bogus_res: CallToolResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(bogus_res.is_error, Some(true));
    assert!(bogus_res.content[0].text.contains("não encontrado"));

    drop(stdin);
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
