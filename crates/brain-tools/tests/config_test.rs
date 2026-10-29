use brain_tools::{Config, ToolProfile};
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
    assert!(config.delivery_ledger, "the ledger is on by default");
    assert_eq!(config.delivery_entries, 512);
    assert_eq!(config.delivery_max_bytes, 8_388_608);
    assert_eq!(config.tool_profile, ToolProfile::Full);
    assert!(config.shell_usage_log, "shell usage log is on by default");
    assert_eq!(
        config.shell_usage_log_path,
        PathBuf::from(".brain/shell-usage.jsonl")
    );
    assert!(!config.shell_usage_log_full);
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
    assert_eq!(
        config.model_path,
        Some(PathBuf::from("/path/to/model.gguf"))
    );
    assert_eq!(config.gpu_layers, 24);
    assert_eq!(config.threads, Some(8));
    assert_eq!(config.default_min_confidence, 0.80);
    assert_eq!(config.default_max_results, 10);
    assert_eq!(config.max_validate_candidates, 48);
}

#[test]
fn the_delivery_ledger_can_be_turned_off() {
    let config = Config::parse_from(["brain-tools", "--delivery-ledger", "false"]);
    assert!(!config.delivery_ledger);
}

#[test]
fn the_tool_profile_can_be_selected_from_the_cli() {
    let config = Config::parse_from(["brain-tools", "--tools", "core"]);
    assert_eq!(config.tool_profile, ToolProfile::Core);
    assert!(ToolProfile::Core.tools().is_some());
    assert!(ToolProfile::Full.tools().is_none());
}

#[test]
fn the_shell_usage_log_can_be_turned_off() {
    let config = Config::parse_from(["brain-tools", "--shell-usage-log", "false"]);
    assert!(!config.shell_usage_log);
}

#[test]
fn the_shell_usage_log_path_is_configurable() {
    let config = Config::parse_from([
        "brain-tools",
        "--shell-usage-log-path",
        "/tmp/shell-usage.jsonl",
    ]);
    assert_eq!(
        config.shell_usage_log_path,
        PathBuf::from("/tmp/shell-usage.jsonl")
    );
}

#[test]
fn the_shell_usage_log_full_mode_is_opt_in() {
    let config = Config::parse_from(["brain-tools", "--shell-usage-log-full"]);
    assert!(config.shell_usage_log_full);
}

#[test]
fn the_shell_guard_is_on_by_default() {
    assert!(Config::parse_from(["brain-tools"]).shell_guard);
}

#[test]
fn the_shell_guard_can_be_turned_off() {
    assert!(!Config::parse_from(["brain-tools", "--shell-guard", "false"]).shell_guard);
}
