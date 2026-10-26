use brain_tools::search::ast_chunker::AstSnippet;
use brain_tools::search::ranker::LexicalRanker;
use std::path::PathBuf;

#[test]
fn test_ranker_prioritizes_symbol_name_over_comment_or_body() {
    let s1_exact_name = AstSnippet {
        file_path: PathBuf::from("src/auth.rs"),
        symbol_name: "authenticate_user".to_string(),
        signature: "pub fn authenticate_user(token: &str) -> bool".to_string(),
        code: "pub fn authenticate_user(token: &str) -> bool {\n    true\n}".to_string(),
        start_line: 1,
        end_line: 3,
    };

    let s2_body_mention = AstSnippet {
        file_path: PathBuf::from("src/controller.rs"),
        symbol_name: "handle_request".to_string(),
        signature: "pub fn handle_request()".to_string(),
        code: "pub fn handle_request() {\n    // call authenticate_user\n    let _ = authenticate_user(\"\");\n}".to_string(),
        start_line: 10,
        end_line: 14,
    };

    let s3_comment_only = AstSnippet {
        file_path: PathBuf::from("src/legacy.rs"),
        symbol_name: "do_something".to_string(),
        signature: "pub fn do_something()".to_string(),
        code: "pub fn do_something() {\n    // old user logic\n}".to_string(),
        start_line: 20,
        end_line: 23,
    };

    let ranker = LexicalRanker::new();
    let scored = ranker.rank("authenticate user", &["authenticate_user", "user"], vec![
        s3_comment_only,
        s2_body_mention,
        s1_exact_name,
    ]);

    assert_eq!(scored.len(), 3);
    // Highest score should be s1_exact_name
    assert_eq!(scored[0].snippet.symbol_name, "authenticate_user");
    assert!(scored[0].score > scored[1].score);
    assert!(scored[1].score > scored[2].score);

    // Verify scores are normalized between 0.0 and 1.0
    for item in &scored {
        assert!(item.score >= 0.0 && item.score <= 1.0);
    }
}
