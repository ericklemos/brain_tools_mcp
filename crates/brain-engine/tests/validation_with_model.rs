//! Regression test for multi-candidate validation.
//!
//! The d3-edge GGUF is a hybrid model (`qwen35`, with recurrent layers). Its
//! recurrent memory refuses to roll back a partial sequence, so the old
//! per-candidate KV reuse silently failed on every candidate after the first:
//! a three-candidate batch validated one and skipped two.
//!
//! It loads the 806 MB model, so it is ignored by default:
//!
//! ```text
//! cargo test -p brain-engine --test validation_with_model -- --ignored
//! ```

use brain_engine::LlamaValidator;
use brain_kernel::ast::AstSnippet;
use std::path::PathBuf;

fn model_path() -> Option<PathBuf> {
    let candidate = std::env::var("BRAIN_TEST_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../models/vllm-d3-edge-Q8_0.gguf")
        });
    candidate.exists().then_some(candidate)
}

fn snippet(symbol: &str) -> AstSnippet {
    AstSnippet {
        file_path: PathBuf::from("src/farming.rs"),
        symbol_name: symbol.to_string(),
        signature: format!("pub fn {symbol}()"),
        code: format!("pub fn {symbol}() {{\n    let _ = 1;\n}}"),
        start_line: 1,
        end_line: 3,
    }
}

#[test]
#[ignore = "loads the 806 MB GGUF model"]
fn every_candidate_is_actually_decoded() {
    let Some(path) = model_path() else {
        eprintln!("skipping: GGUF model not found (set BRAIN_TEST_MODEL)");
        return;
    };

    let validator = LlamaValidator::new(&path, 0).expect("model must load");
    let candidates = vec![
        snippet("assign_farmers"),
        snippet("harvest_crops"),
        snippet("plant_seed"),
    ];

    // Threshold 0.0 accepts whichever candidates the model manages to judge, so
    // the assertion measures decoding, not relevance.
    let report = validator
        .validate_batch("como funciona a plantação", candidates, 0.0)
        .expect("validation must not abort");

    assert_eq!(
        report.skipped, 0,
        "the model failed to judge {} candidate(s); multi-candidate validation is broken again",
        report.skipped
    );
    assert_eq!(
        report.scored.len(),
        3,
        "every candidate must be decoded, not only the first"
    );
}
