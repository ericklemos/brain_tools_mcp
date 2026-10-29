use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use brain_kernel::protocol::{JsonRpcResponse, ListToolsResult};

/// The wiring is not real until the binary lists the tool. This spawns the
/// actual server, which is the only place `plugin::install` and
/// `Registry::new` adoption meet.
#[test]
fn brain_fetch_is_registered_in_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_brain-tools");
    let mut child = Command::new(bin)
        .current_dir(dir.path())
        .arg("--root-dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn brain-tools");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).expect("tools/list response");
    let response: JsonRpcResponse = serde_json::from_str(line.trim()).expect("valid JSON-RPC");
    let list: ListToolsResult =
        serde_json::from_value(response.result.expect("result")).expect("tool list");

    let tool = list
        .tools
        .iter()
        .find(|tool| tool.name == "brain_fetch")
        .expect("brain_fetch must be registered");

    assert_eq!(tool.input_schema["required"][0], "url");
    assert!(tool.description.contains("brain_peek"));

    let _ = child.kill();
    let _ = child.wait();
}
