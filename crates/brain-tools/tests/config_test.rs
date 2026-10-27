use brain_tools::Config;
use clap::Parser;
use std::path::PathBuf;

#[test]
fn test_default_config() {
    let config = Config::parse_from(["brain-tools"]);
    assert_eq!(config.root_dir, PathBuf::from("."));
    assert_eq!(config.model_path, None);
    assert_eq!(config.gpu_layers, 33);
    assert_eq!(config.threads, None);
    assert_eq!(config.log_level, "info");
    assert_eq!(config.default_min_confidence, 0.65);
    assert_eq!(config.default_max_results, 5);
    assert_eq!(config.max_validate_candidates, 24);
}

#[test]
fn test_custom_cli_args() {
    let config = Config::parse_from([
        "brain-tools",
        "--root-dir",
        "/tmp/project",
        "--model-path",
        "/path/to/model.gguf",
        "--gpu-layers",
        "24",
        "--threads",
        "8",
        "--default-min-confidence",
        "0.80",
        "--default-max-results",
        "10",
        "--max-validate-candidates",
        "48",
    ]);

    assert_eq!(config.root_dir, PathBuf::from("/tmp/project"));
    assert_eq!(config.model_path, Some(PathBuf::from("/path/to/model.gguf")));
    assert_eq!(config.gpu_layers, 24);
    assert_eq!(config.threads, Some(8));
    assert_eq!(config.default_min_confidence, 0.80);
    assert_eq!(config.default_max_results, 10);
    assert_eq!(config.max_validate_candidates, 48);
}
