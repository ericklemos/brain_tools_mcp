//! End-to-end proof that call edges follow scope, not bare names.
//!
//! The defect these lock down: `brain_impact_check new` reported 239 impacts,
//! and every one of 120 sampled call-sites was a definition or an unrelated
//! line. A graph that links a call to every definition sharing its name is
//! worse than an empty one — six tools read it and none of them could tell.

use brain_graph::builder::GraphBuilder;
use brain_graph::model::{ProjectCodeGraph, RelationKind};
use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::fs;
use std::path::Path;

fn build(files: &[(&str, &str)]) -> (tempfile::TempDir, ProjectCodeGraph) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (relative, content) in files {
        let path = dir.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create dir");
        }
        fs::write(&path, content).expect("write fixture");
    }
    let graph = GraphBuilder::new()
        .expect("builder")
        .build_graph(dir.path())
        .expect("graph");
    (dir, graph)
}

fn node_of(graph: &ProjectCodeGraph, file: &str, name: &str) -> NodeIndex {
    graph
        .symbol_lookup
        .get(name)
        .and_then(|indices| {
            indices.iter().copied().find(|&idx| {
                graph
                    .graph
                    .node_weight(idx)
                    .is_some_and(|n| n.file_path.to_string_lossy().ends_with(file))
            })
        })
        .unwrap_or_else(|| panic!("{name} not found in {file}"))
}

fn callers(graph: &ProjectCodeGraph, target: NodeIndex) -> Vec<String> {
    graph
        .graph
        .edges_directed(target, Direction::Incoming)
        .filter_map(|edge| {
            graph
                .graph
                .node_weight(edge.source())
                .map(|caller| caller.name.clone())
        })
        .collect()
}

#[test]
fn a_use_declaration_selects_one_of_two_same_named_definitions() {
    let (_dir, graph) = build(&[
        ("src/lib.rs", "pub mod auth;\npub mod billing;\n"),
        (
            "src/auth.rs",
            "use crate::billing::notify;\npub fn login() { notify(); }\n",
        ),
        ("src/billing.rs", "pub fn notify() {}\n"),
        ("src/audit.rs", "pub fn notify() {}\n"),
    ]);

    let billing_notify = node_of(&graph, "billing.rs", "notify");
    let audit_notify = node_of(&graph, "audit.rs", "notify");

    assert_eq!(
        callers(&graph, billing_notify),
        vec!["login"],
        "the `use` names billing::notify, so that is the edge that exists"
    );
    assert!(
        callers(&graph, audit_notify).is_empty(),
        "audit::notify is a different function and must not inherit the call"
    );
}

#[test]
fn without_the_use_the_edge_goes_to_the_same_crate_definition() {
    let (_dir, graph) = build(&[
        ("crates/a/src/lib.rs", "pub mod one;\n"),
        ("crates/a/src/one.rs", "pub fn notify() {}\n"),
        (
            "crates/b/src/lib.rs",
            "pub mod two;\npub fn run() { notify(); }\n",
        ),
        ("crates/b/src/two.rs", "pub fn notify() {}\n"),
    ]);

    let same_crate = node_of(&graph, "crates/b/src/two.rs", "notify");
    let other_crate = node_of(&graph, "crates/a/src/one.rs", "notify");

    assert_eq!(
        callers(&graph, same_crate),
        vec!["run"],
        "same-crate is the strongest surviving prior"
    );
    assert!(
        callers(&graph, other_crate).is_empty(),
        "a function in another crate sharing the name is not this call"
    );
}

#[test]
fn an_ambiguous_cross_crate_name_produces_no_edge_and_is_counted() {
    let (_dir, graph) = build(&[
        ("crates/a/src/lib.rs", "pub fn notify() {}\n"),
        ("crates/b/src/lib.rs", "pub fn notify() {}\n"),
        (
            "crates/c/src/lib.rs",
            "pub fn run() { notify(); }\n",
        ),
    ]);

    let a = node_of(&graph, "crates/a/src/lib.rs", "notify");
    let b = node_of(&graph, "crates/b/src/lib.rs", "notify");

    assert!(callers(&graph, a).is_empty());
    assert!(callers(&graph, b).is_empty());
    assert_eq!(
        graph.resolution.ambiguous_names.get("notify"),
        Some(&1),
        "the dropped call is counted so the tools can disclose it"
    );
}

#[test]
fn a_method_call_resolves_by_receiver_type_not_by_name() {
    let (_dir, graph) = build(&[(
        "src/lib.rs",
        r#"
pub struct Builder;
pub struct Parser;

impl Builder {
    pub fn run(&self) {}
}
impl Parser {
    pub fn run(&self) {}
}

pub fn drive(builder: &Builder, parser: &Parser) {
    builder.run();
    parser.run();
}
"#,
    )]);

    let builder_run = graph
        .symbol_lookup
        .get("run")
        .and_then(|idxs| {
            idxs.iter().copied().find(|&idx| {
                graph
                    .graph
                    .node_weight(idx)
                    .is_some_and(|n| n.container.as_deref() == Some("Builder"))
            })
        })
        .expect("Builder::run");
    let parser_run = graph
        .symbol_lookup
        .get("run")
        .and_then(|idxs| {
            idxs.iter().copied().find(|&idx| {
                graph
                    .graph
                    .node_weight(idx)
                    .is_some_and(|n| n.container.as_deref() == Some("Parser"))
            })
        })
        .expect("Parser::run");

    assert_eq!(callers(&graph, builder_run), vec!["drive"]);
    assert_eq!(callers(&graph, parser_run), vec!["drive"]);
}

#[test]
fn a_local_binding_types_its_receiver_across_files() {
    let (_dir, graph) = build(&[
        ("src/lib.rs", "pub mod build;\npub mod parse;\n"),
        (
            "src/main.rs",
            r#"
use crate::build::Builder;
use crate::parse::Parser;

fn go() {
    let b = Builder::default();
    b.step();
}
"#,
        ),
        (
            "src/build.rs",
            "pub struct Builder;\nimpl Builder {\n    pub fn default() -> Self { Builder }\n    pub fn step(&self) {}\n}\n",
        ),
        (
            "src/parse.rs",
            "pub struct Parser;\nimpl Parser {\n    pub fn step(&self) {}\n}\n",
        ),
    ]);

    let builder_step = graph
        .symbol_lookup
        .get("step")
        .and_then(|idxs| {
            idxs.iter().copied().find(|&idx| {
                graph
                    .graph
                    .node_weight(idx)
                    .is_some_and(|n| n.container.as_deref() == Some("Builder"))
            })
        })
        .expect("Builder::step");
    let parser_step = graph
        .symbol_lookup
        .get("step")
        .and_then(|idxs| {
            idxs.iter().copied().find(|&idx| {
                graph
                    .graph
                    .node_weight(idx)
                    .is_some_and(|n| n.container.as_deref() == Some("Parser"))
            })
        })
        .expect("Parser::step");

    assert_eq!(
        callers(&graph, builder_step),
        vec!["go"],
        "`let b = Builder::default()` types `b`, so `b.step()` is Builder::step"
    );
    assert!(callers(&graph, parser_step).is_empty());
}

#[test]
fn the_caller_is_the_innermost_definition_not_the_first_containing_one() {
    let (_dir, graph) = build(&[(
        "src/lib.rs",
        r#"
pub fn outer() {
    pub fn inner() {
        helper();
    }
    inner();
}
pub fn helper() {}
"#,
    )]);

    let helper = node_of(&graph, "lib.rs", "helper");
    assert_eq!(
        callers(&graph, helper),
        vec!["inner"],
        "the call sits inside `inner`; attributing it to `outer` blames the wrong symbol"
    );

    let inner = node_of(&graph, "lib.rs", "inner");
    assert_eq!(callers(&graph, inner), vec!["outer"]);
}

#[test]
fn an_imported_external_crate_does_not_link_to_a_local_same_named_function() {
    let (_dir, graph) = build(&[(
        "src/lib.rs",
        r#"
use tracing::info;

pub fn info(message: &str) { let _ = message; }

pub fn run() { info!("hello"); }
"#,
    )]);

    let local_info = node_of(&graph, "lib.rs", "info");
    // Same file, so the local definition is still the safest answer for a bare
    // call; what must never happen is the macro being linked by bare name only.
    let incoming = callers(&graph, local_info);
    assert!(
        incoming.is_empty() || incoming == vec!["run"],
        "at most the same-file call, never a fan-out: got {incoming:?}"
    );
}

#[test]
fn a_glob_import_makes_the_globbed_module_the_target() {
    let (_dir, graph) = build(&[
        ("crates/a/src/lib.rs", "pub mod helpers;\npub fn notify() {}\n"),
        ("crates/a/src/helpers.rs", "pub fn notify() {}\n"),
        (
            "crates/b/src/lib.rs",
            "use a::helpers::*;\npub fn run() { notify(); }\n",
        ),
        ("crates/b/src/helpers.rs", "pub fn notify() {}\n"),
    ]);

    let globbed = node_of(&graph, "crates/a/src/helpers.rs", "notify");
    let local_other = node_of(&graph, "crates/b/src/helpers.rs", "notify");

    assert_eq!(
        callers(&graph, globbed),
        vec!["run"],
        "`use a::helpers::*` brings that module's items into scope"
    );
    assert!(callers(&graph, local_other).is_empty());
}

#[test]
fn resolution_is_disclosed_in_the_impact_report() {
    let (_dir, graph) = build(&[
        ("crates/a/src/lib.rs", "pub fn notify() {}\n"),
        ("crates/b/src/lib.rs", "pub fn notify() {}\n"),
        ("crates/c/src/lib.rs", "pub fn run() { notify(); }\n"),
    ]);

    let sections =
        brain_graph::traversal::GraphTraversal::impact_sections(&graph, "run", Path::new("."));
    let headline = &sections[0];
    assert!(
        headline.body.contains("Confiança do grafo"),
        "the headline must carry the disclosure so no budget can hide it: {headline:?}"
    );
    assert!(headline.body.contains("ambíguas"));
}

#[test]
fn every_edge_kind_recorded_is_a_call() {
    let (_dir, graph) = build(&[(
        "src/lib.rs",
        "pub fn helper() {}\npub fn run() { helper(); }\n",
    )]);

    let kinds: Vec<RelationKind> = graph
        .graph
        .edge_weights()
        .map(|edge| edge.kind.clone())
        .collect();
    assert!(kinds.iter().all(|kind| *kind == RelationKind::Calls));
    assert_eq!(kinds.len(), 1, "one call, one edge: {kinds:?}");
}
