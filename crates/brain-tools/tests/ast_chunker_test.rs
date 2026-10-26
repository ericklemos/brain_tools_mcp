use brain_tools::search::ast_chunker::AstChunker;
use std::path::Path;

#[test]
fn test_ast_chunker_rust_function_and_struct() {
    let rust_code = r#"
/// Verifies a user authentication token.
pub fn verify_token(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    raw.starts_with("Bearer ")
}

pub struct AuthService {
    secret: String,
}
"#;

    let chunker = AstChunker::new();
    let path = Path::new("src/auth.rs");

    // Match at line 5: "return false;" (inside verify_token)
    let snippets = chunker
        .extract_snippets(path, rust_code, &[5])
        .expect("Extraction failed");

    assert_eq!(snippets.len(), 1);
    let s = &snippets[0];
    assert_eq!(s.symbol_name, "verify_token");
    assert!(s.signature.contains("pub fn verify_token(raw: &str) -> bool"));
    assert!(s.code.contains("pub fn verify_token"));
    assert_eq!(s.start_line, 3);
    assert_eq!(s.end_line, 8);

    // Match at line 11: "secret: String" (inside AuthService struct)
    let struct_snippets = chunker
        .extract_snippets(path, rust_code, &[11])
        .expect("Extraction failed");
    assert_eq!(struct_snippets.len(), 1);
    assert_eq!(struct_snippets[0].symbol_name, "AuthService");
}

#[test]
fn test_ast_chunker_kotlin_class_and_function() {
    let kotlin_code = r#"
package com.example.service

class UserService {
    fun authenticate(token: String): Boolean {
        val valid = token.isNotEmpty()
        return valid
    }
}
"#;

    let chunker = AstChunker::new();
    let path = Path::new("src/UserService.kt");

    // Match at line 6: "val valid = ..." (inside authenticate function)
    let snippets = chunker
        .extract_snippets(path, kotlin_code, &[6])
        .expect("Extraction failed");

    assert_eq!(snippets.len(), 1);
    let s = &snippets[0];
    assert_eq!(s.symbol_name, "authenticate");
    assert!(s.signature.contains("fun authenticate(token: String): Boolean"));
    assert_eq!(s.start_line, 5);
    assert_eq!(s.end_line, 8);
}

#[test]
fn test_ast_chunker_deduplication() {
    let rust_code = r#"
pub fn calculate() {
    let a = 1;
    let b = 2;
    let c = a + b;
}
"#;
    let chunker = AstChunker::new();
    let path = Path::new("src/math.rs");

    // Two matches in the same function (lines 3 and 4)
    let snippets = chunker
        .extract_snippets(path, rust_code, &[3, 4])
        .expect("Extraction failed");

    assert_eq!(snippets.len(), 1, "Should deduplicate matches in the same node");
}
