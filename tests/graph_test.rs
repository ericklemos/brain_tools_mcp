use brain_tools::graph::builder::GraphBuilder;
use brain_tools::graph::model::RelationKind;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_graph_builder_and_incremental_update() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let crypto_file = src.join("crypto.rs");
    fs::write(
        &crypto_file,
        r#"
pub fn hash_password(plain: &str) -> String {
    format!("hashed_{plain}")
}
"#,
    )
    .unwrap();

    let auth_file = src.join("auth.rs");
    fs::write(
        &auth_file,
        r#"
pub fn register_user(plain: &str) -> String {
    let hashed = hash_password(plain);
    hashed
}
"#,
    )
    .unwrap();

    let kt_file = src.join("UserService.kt");
    fs::write(
        &kt_file,
        r#"
class UserService {
    fun setupPassword(pwd: String): String {
        return hash_password(pwd)
    }
}
"#,
    )
    .unwrap();

    let builder = GraphBuilder::new().expect("Builder init failed");
    let mut code_graph = builder.build_graph(root).expect("Graph build failed");

    // 1. Check nodes
    assert!(code_graph.symbol_lookup.contains_key("hash_password"));
    assert!(code_graph.symbol_lookup.contains_key("register_user"));
    assert!(code_graph.symbol_lookup.contains_key("setupPassword"));

    let crypto_node_idx = code_graph.symbol_lookup.get("hash_password").unwrap()[0];
    let auth_node_idx = code_graph.symbol_lookup.get("register_user").unwrap()[0];

    // 2. Check edge from register_user -> hash_password
    let edge_idx = code_graph.graph.find_edge(auth_node_idx, crypto_node_idx);
    assert!(edge_idx.is_some(), "Expected Calls edge between auth and crypto");
    let edge = code_graph.graph.edge_weight(edge_idx.unwrap()).unwrap();
    assert_eq!(edge.kind, RelationKind::Calls);

    // 3. Test incremental update: update crypto.rs with new function
    fs::write(
        &crypto_file,
        r#"
pub fn hash_password(plain: &str) -> String {
    format!("v2_{plain}")
}

pub fn generate_salt() -> String {
    "salt123".to_string()
}
"#,
    )
    .unwrap();

    let changed = builder
        .update_file(&mut code_graph, &crypto_file, root)
        .expect("Update failed");
    assert!(changed);
    assert!(code_graph.symbol_lookup.contains_key("generate_salt"));
}
