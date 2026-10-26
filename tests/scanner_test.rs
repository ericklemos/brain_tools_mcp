use brain_tools::search::scanner::LexicalScanner;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_lexical_scanner_finds_in_rs_and_kt_and_respects_ignore() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    let target = root.join("target");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&target).unwrap();

    let rs_file = src.join("main.rs");
    fs::write(
        &rs_file,
        "// Main entry point\npub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.15\n}\n",
    )
    .unwrap();

    let kt_file = src.join("Service.kt");
    fs::write(
        &kt_file,
        "package com.example\n\nclass PaymentService {\n    fun calculate_tax(amount: Double): Double {\n        return amount * 0.15\n    }\n}\n",
    )
    .unwrap();

    // In target folder (should be ignored)
    let target_file = target.join("generated.rs");
    fs::write(&target_file, "pub fn calculate_tax() {}\n").unwrap();

    // Txt file (should be ignored by code scanner)
    let txt_file = root.join("notes.txt");
    fs::write(&txt_file, "calculate_tax notes here").unwrap();

    let scanner = LexicalScanner::new(root);
    let matches = scanner.search_keywords(&["calculate_tax"]).unwrap();

    // Should only find matches in src/main.rs and src/Service.kt
    assert_eq!(matches.len(), 2, "Expected matches in 2 files, got {:?}", matches);

    let file_names: Vec<String> = matches
        .iter()
        .map(|m| m.file_path.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(file_names.contains(&"main.rs".to_string()));
    assert!(file_names.contains(&"Service.kt".to_string()));
    assert!(!file_names.contains(&"generated.rs".to_string()));
    assert!(!file_names.contains(&"notes.txt".to_string()));
}
