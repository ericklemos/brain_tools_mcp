//! End-to-end: a real `cargo` run on a scratch crate, through the public tool.
//!
//! These tests spawn cargo, so they are slower than the unit tests. They exist
//! because the parser's fixtures are only a promise that the *format* is
//! understood; this checks the promise against the tool that actually runs.

use brain_exec::{ExecTool, ExecParams};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::store::ResponseStore;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn tool_for(root: &Path) -> ExecTool {
    tool_and_store(root).0
}

/// Returns the store too, so a test can read back what truncation left behind.
fn tool_and_store(root: &Path) -> (ExecTool, Arc<ResponseStore>) {
    let store = Arc::new(ResponseStore::new(Duration::from_secs(60), 4 * 1024 * 1024));
    let tool = ExecTool::new(root, store.clone(), 4000, Duration::from_secs(120));
    (tool, store)
}

fn scratch_crate(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"scratch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    for (path, content) in files {
        fs::write(dir.path().join(path), content).unwrap();
    }
    dir
}

async fn run(tool: &ExecTool, kind: &str) -> String {
    let out = tool
        .run(ExecParams {
            kind: kind.to_string(),
            budget_tokens: None,
            refresh: true,
        })
        .await;
    assert_eq!(out.is_error, None, "tool returned an error");
    out.content[0].text.clone()
}

#[tokio::test]
async fn a_broken_crate_reports_the_error_grouped_not_raw() {
    let dir = scratch_crate(&[(
        "src/main.rs",
        "fn main() {\n    let a: u32 = \"text\";\n    let b: u32 = 1.5;\n    let _ = (a, b);\n}\n",
    )]);
    let tool = tool_for(dir.path());

    let text = run(&tool, "check").await;

    assert!(text.contains("E0308"), "expected the error code: {text}");
    assert!(text.contains("2 ocorrência(s)"), "same code must group: {text}");
    assert!(text.contains("src/main.rs"), "expected the file: {text}");
    // The raw compiler banner never reaches the agent.
    assert!(!text.contains("Checking scratch"), "raw cargo log leaked: {text}");
    assert!(!text.contains("For more information about this error"), "failure-note leaked: {text}");
}

#[tokio::test]
async fn a_green_crate_says_ok_in_one_headline() {
    let dir = scratch_crate(&[("src/lib.rs", "pub fn add(a: u32, b: u32) -> u32 { a + b }\n")]);
    let tool = tool_for(dir.path());

    let text = run(&tool, "check").await;

    assert!(text.contains("`check`"), "{text}");
    assert!(text.contains("ok"), "{text}");
    assert!(!text.contains("erro"), "{text}");
}

#[tokio::test]
async fn a_failing_test_arrives_with_the_assert_intact() {
    let dir = scratch_crate(&[(
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\
         \x20   #[test]\n    fn sums() { assert_eq!(add(2, 2), 5); }\n\
         \x20   #[test]\n    fn ok_one() { assert_eq!(add(1, 1), 2); }\n}\n",
    )]);
    let tool = tool_for(dir.path());

    let text = run(&tool, "test").await;

    assert!(text.contains("1 falharam, 1 passaram"), "expected totals: {text}");
    assert!(text.contains("sums"), "expected the failing test name: {text}");
    assert!(text.contains("left: 4"), "expected the assert values: {text}");
    assert!(text.contains("right: 5"), "expected the assert values: {text}");
}

#[tokio::test]
async fn compile_errors_and_test_failures_are_reported_from_a_single_run() {
    // A crate that compiles in the lib but whose test body does not.
    let dir = scratch_crate(&[(
        "src/lib.rs",
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\
         #[cfg(test)]\nmod tests {\n\
         \x20   #[test]\n    fn broken() { let x: u32 = \"nope\"; let _ = x; }\n}\n",
    )]);
    let tool = tool_for(dir.path());

    let text = run(&tool, "test").await;

    assert!(text.contains("E0308"), "compile error inside tests must appear: {text}");
    assert!(text.contains("erro"), "{text}");
}

#[tokio::test]
async fn warnings_are_a_digest_and_do_not_hide_a_successful_build() {
    let dir = scratch_crate(&[(
        "src/lib.rs",
        "fn never_used() {}\nfn also_never_used() {}\npub fn used() -> u32 { 1 }\n",
    )]);
    let tool = tool_for(dir.path());

    let text = run(&tool, "check").await;

    assert!(text.contains("ok"), "warnings must not read as failure: {text}");
    assert!(text.contains("Warnings —"), "{text}");
    assert!(text.contains("dead_code"), "{text}");
}

#[tokio::test]
async fn a_second_call_without_changes_is_served_from_cache() {
    let dir = scratch_crate(&[("src/lib.rs", "pub fn add(a: u32, b: u32) -> u32 { a + b }\n")]);
    let tool = tool_for(dir.path());

    let first = run(&tool, "check").await;
    assert!(!first.contains("(cache)"), "{first}");

    let second = tool
        .run(ExecParams {
            kind: "check".to_string(),
            budget_tokens: None,
            refresh: false,
        })
        .await
        .content[0]
        .text
        .clone();

    assert!(second.contains("(cache)"), "expected a cache hit: {second}");
}

/// The spec's first success criterion: forty errors must fit the budget and
/// still name every affected file, with the rest recoverable.
#[tokio::test]
async fn forty_errors_fit_a_small_budget_and_stay_recoverable() {
    let body: String = (0..40)
        .map(|i| format!("    let v{i}: u32 = \"bad{i}\";\n"))
        .collect();
    let source = format!("fn main() {{\n{body}}}\n");
    let dir = scratch_crate(&[("src/main.rs", &source)]);
    let (tool, store) = tool_and_store(dir.path());

    let out = tool
        .run(ExecParams {
            kind: "check".to_string(),
            budget_tokens: Some(400),
            refresh: true,
        })
        .await;
    let text = &out.content[0].text;

    assert!(text.contains("40 erro(s)"), "headline must survive: {text}");
    assert!(text.contains("src/main.rs"), "file must be named: {text}");
    assert!(text.contains("Resposta truncada"), "budget must bite: {text}");
    assert!(text.contains("brain_peek"), "omission must be recoverable: {text}");

    // The cap is real: an unbudgeted run of the same crate is far larger.
    assert!(
        text.len() < 4000,
        "response must respect the budget, got {} chars",
        text.len()
    );

    // The omitted blocks are all there — nothing was lost, only deferred.
    let handle = text
        .split("handle: `")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .expect("handle in envelope");
    let omitted = store.get(handle).expect("handle resolves");
    // Nothing is lost, only deferred: the response plus the stored body must
    // account for all forty occurrences. How many fit depends on the budget, so
    // the assertion is on the union, not on where the cut landed.
    let complete = format!("{text}{omitted}");
    for n in 0..40 {
        assert!(
            complete.contains(&format!("\"bad{n}\"")),
            "occurrence {n} is neither shown nor recoverable"
        );
    }
}

#[tokio::test]
async fn an_invalid_kind_is_rejected_without_running_anything() {
    let dir = scratch_crate(&[("src/lib.rs", "pub fn f() {}\n")]);
    let tool = tool_for(dir.path());

    let out = tool
        .run(ExecParams {
            kind: "rm -rf /".to_string(),
            budget_tokens: None,
            refresh: false,
        })
        .await;

    assert_eq!(out.is_error, Some(true));
    assert!(out.content[0].text.contains("kind inválido"));
}

#[tokio::test]
async fn the_plugin_announces_its_name_and_the_closed_enum() {
    let dir = scratch_crate(&[("src/lib.rs", "pub fn f() {}\n")]);
    let tool = tool_for(dir.path());

    assert_eq!(tool.name(), "brain_exec");
    let schema = tool.schema();
    assert_eq!(schema.name, "brain_exec");
    let kinds = &schema.input_schema["properties"]["kind"]["enum"];
    assert_eq!(
        kinds,
        &serde_json::json!(["build", "check", "test", "clippy"])
    );
}
