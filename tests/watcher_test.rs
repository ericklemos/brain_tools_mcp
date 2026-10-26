use brain_tools::graph::builder::GraphBuilder;
use brain_tools::graph::watcher::GraphWatcher;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::RwLock;

#[tokio::test]
async fn test_watcher_updates_graph_on_file_modification() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let rs_file = src.join("service.rs");
    fs::write(
        &rs_file,
        r#"
pub fn initial_func() {}
"#,
    )
    .unwrap();

    let builder = Arc::new(GraphBuilder::new().expect("Builder init failed"));
    let graph = Arc::new(RwLock::new(
        builder.build_graph(root).expect("Graph build failed"),
    ));

    {
        let lock = graph.read().await;
        assert!(lock.symbol_lookup.contains_key("initial_func"));
        assert!(!lock.symbol_lookup.contains_key("added_later_func"));
    }

    let _watcher =
        GraphWatcher::start(root.to_path_buf(), graph.clone(), builder).expect("Watcher start failed");

    // Modify file
    fs::write(
        &rs_file,
        r#"
pub fn initial_func() {}
pub fn added_later_func() {}
"#,
    )
    .unwrap();

    // Wait for debounce and async update
    let mut detected = false;
    for _ in 0..20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let lock = graph.read().await;
        if lock.symbol_lookup.contains_key("added_later_func") {
            detected = true;
            break;
        }
    }

    assert!(detected, "Watcher did not update graph within timeout");
}
