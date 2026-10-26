use brain_tools::engine::ValidationClient;
use brain_tools::search::ast_chunker::AstSnippet;
use std::path::PathBuf;

#[tokio::test]
async fn test_validation_actor_runs_fallback_without_model() {
    // Pass None as model path -> should gracefully use LexicalRanker in the actor thread
    let client = ValidationClient::new(None, 0);

    let s1 = AstSnippet {
        file_path: PathBuf::from("src/jwt.rs"),
        symbol_name: "verify_token".to_string(),
        signature: "pub fn verify_token(raw: &str) -> bool".to_string(),
        code: "pub fn verify_token(raw: &str) -> bool { true }".to_string(),
        start_line: 1,
        end_line: 3,
    };

    let s2 = AstSnippet {
        file_path: PathBuf::from("src/unrelated.rs"),
        symbol_name: "draw_canvas".to_string(),
        signature: "pub fn draw_canvas()".to_string(),
        code: "pub fn draw_canvas() {}".to_string(),
        start_line: 10,
        end_line: 12,
    };

    let results = client
        .validate(
            "verify jwt authentication token".to_string(),
            vec!["verify_token".to_string(), "jwt".to_string()],
            vec![s2, s1],
            0.5,
            5,
        )
        .await
        .expect("Validation failed");

    assert!(!results.is_empty());
    assert_eq!(results[0].snippet.symbol_name, "verify_token");
    assert!(results[0].score >= 0.5);
}
