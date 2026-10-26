# brain_edit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the agent edit a symbol's body by name — `path` + `symbol` + `expected_hash` + `new_body` — instead of a whole-file `Read` plus an `Edit` with a literal anchor.

**Architecture:** A new `SpanResolver` in `brain-graph` parses the file with the same tree-sitter grammars the chunker uses, but keeps `start_byte`/`end_byte` and descends into `impl`/`mod`/`trait` containers. A new `EditTool` hashes the file, resolves the span, splices bytes in memory, re-parses, writes atomically, then re-reads from disk to prove what it reports. `brain_read` starts printing the blake3 prefix that `expected_hash` consumes.

**Tech Stack:** Rust 2021, `tree-sitter` + `tree-sitter-rust` (already in `brain-graph`), `blake3` (already a dependency), `brain_kernel::budget` for sections and `ResponseStore` for overflow, `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-10-26-brain-edit-design.md`

## Global Constraints

- `new_body` replaces the whole `{ … }` block, braces included. The splice is raw bytes: no reindentation, no formatting, no touching anything outside the span.
- `expected_hash` is **required**. It is the blake3 hex of the whole file; a 12-character prefix is accepted. Mismatch never writes.
- The daemon never formats. `rustfmt`/`ktlint` are not called.
- **Kotlin is out of scope for this plan.** `SpanResolver` may return an empty symbol list for `.kt`; the tool then reports `NotSupported`.
- Every failure is `CallToolResult::error` — never text that could read as success.
- No new crate dependencies in `brain-graph` (`blake3`, `tree-sitter`, `tempfile` are already there).
- All user-facing strings are in Portuguese, matching every other tool in this repo.
- `tools/list` order must stay: `brain_edit` registers after `brain_read`, before `brain_map`.
- The `Tool` struct (`crates/brain-kernel/src/protocol.rs:99-104`) has no `annotations` field. **Do not add one in this plan** — it would change the serialized `tools/list` of all 11 existing tools.

## Review Focus

Inputs and conditions the spec implies but no task's happy path covers, in order of how likely they are to bite:

1. **A symbol whose body is not a `{ }` block** — trait `fn foo();`, `struct Unit;`, a Kotlin `val x = 1`. The tool must say "no body" and write nothing, not silently edit the whole item.
2. **A `new_body` that closes one brace too many** — the file still parses (the extra brace becomes a sibling statement's error only sometimes), so a naive re-parse misses it. The post-write symbol re-resolution must catch that the symbol's span no longer matches.
3. **A file that changes between the hash check and the rename** — an editor's format-on-save. The write must not clobber it. The rename is atomic but the check-then-write window is real; the tool must re-hash immediately before writing.
4. **A symbol name that exists in two sibling `impl` blocks** — `impl Foo::new` and `impl Bar::new`. Must refuse and list both, never pick the first.
5. **CRLF line endings** — `\r\n`. Byte offsets are on the raw text; any line arithmetic that splits on `\n` must not count `\r` as content, and the untouched part of the file must keep its `\r`.

---

### Task 1: `SpanResolver` — symbols with byte ranges

**Files:**
- Create: `crates/brain-graph/src/span.rs`
- Modify: `crates/brain-graph/src/lib.rs` (add `pub mod span;`)
- Test: inline `#[cfg(test)] mod tests` in `span.rs`

**Interfaces:**
- Produces:
  - `pub struct SymbolSpan { pub name: String, pub container: Option<String>, pub head_range: Range<usize>, pub body_range: Range<usize>, pub start_line: usize, pub end_line: usize }`
  - `pub struct SpanResolver;`
  - `impl SpanResolver { pub fn new() -> Self; pub fn symbols(&self, path: &Path, content: &str) -> Result<Vec<SymbolSpan>>; }`
  - `pub fn offset_to_line(content: &str, offset: usize) -> usize` (1-based, `\r\n` safe)
- Consumes: `brain_kernel::ast::SupportedLanguage::from_path`, `brain_kernel::Result`.

- [ ] **Step 1: Write the failing test — a top-level function's body range covers exactly its braces**

```rust
// tests module of span.rs
#[test]
fn a_top_level_function_body_range_covers_exactly_the_braces() {
    let content = "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    !raw.is_empty()\n}\n";
    let spans = SpanResolver::new().symbols(Path::new("auth.rs"), content).unwrap();
    let span = spans.iter().find(|s| s.name == "verify").unwrap();

    assert_eq!(&content[span.body_range.clone()], "{\n    !raw.is_empty()\n}");
    assert!(content[span.head_range.clone()].ends_with("bool "));
    assert_eq!(span.start_line, 3);
    assert_eq!(span.end_line, 5);
    assert_eq!(span.container, None);
}
```

- [ ] **Step 2: Run it and watch it fail to compile**

Run: `cargo test -p brain-graph span:: --no-run`
Expected: `error[E0433]: failed to resolve: use of undeclared crate or module` — `SpanResolver` does not exist.

- [ ] **Step 3: Write `SpanResolver::symbols`**

Parse with `SupportedLanguage::from_path`; for `.kt` return `Ok(Vec::new())` (out of scope). Collect candidate nodes recursively — `function_item`, `impl_item`, `struct_item`, `enum_item`, `trait_item`, `mod_item` for Rust — and do **not** recurse into a node once it is added as a symbol *unless* it is a container (`impl_item`, `trait_item`, `mod_item`), whose children are separate symbols with `container` set to `impl <type text>` / `trait <name>` / `mod <name>`.

For each node:
- `body_range` comes from `node.child_by_field_name("body")`, `start_byte()..end_byte()`; if absent, skip the node (Task 2 handles `NoBody`, this task just omits it).
- `head_range` is `node.start_byte()..body.start_byte()`.
- `start_line`/`end_line` come from `offset_to_line` on `body_range`, not `end_position().row + 1`.

`offset_to_line` counts `\n` before the offset and adds 1; for an offset landing after a `\r\n`, the answer is the same as for the `\n`, which is what makes it CRLF-safe.

- [ ] **Step 4: Run the test**

Run: `cargo test -p brain-graph span::a_top_level_function`
Expected: PASS.

- [ ] **Step 5: Write the failing test — a method inside `impl` resolves to the method, not the `impl`**

```rust
#[test]
fn a_method_inside_an_impl_resolves_to_the_method() {
    let content = "impl Auth {\n    pub fn verify(&self) -> bool {\n        true\n    }\n\n    pub fn logout(&self) {}\n}\n";
    let spans = SpanResolver::new().symbols(Path::new("auth.rs"), content).unwrap();

    let verify = spans.iter().find(|s| s.name == "verify").unwrap();
    assert_eq!(&content[verify.body_range.clone()], "{\n        true\n    }");
    assert_eq!(verify.container, Some("Auth".to_string()));

    let logout = spans.iter().find(|s| s.name == "logout").unwrap();
    assert_eq!(&content[logout.body_range.clone()], "{}");
    assert!(spans.iter().any(|s| s.container.is_none()), "the impl itself is a symbol");
}
```

- [ ] **Step 6: Run it**

Run: `cargo test -p brain-graph span::a_method_inside`
Expected: FAIL — `container` is `None` because descent is not implemented yet.

- [ ] **Step 7: Implement the descent and the container name**

Recurse into container nodes, passing the container's display name down: for `impl_item` use the `type` field text (`Auth`), for `trait_item` and `mod_item` use the `name` field text. Local symbols get `container: Some(that)`; the container node itself keeps `container: None` but still contributes a `SymbolSpan` (its body is the whole `{ … }`).

- [ ] **Step 8: Run the tests**

Run: `cargo test -p brain-graph span::`
Expected: both PASS.

- [ ] **Step 9: Write the failing tests — CRLF offsets and their line numbers**

```rust
#[test]
fn crlf_content_keeps_byte_offsets_and_reports_lines_without_counting_cr() {
    let content = "fn a() {\r\n    b();\r\n}\r\n\r\nfn c() {}\r\n";
    let spans = SpanResolver::new().symbols(Path::new("x.rs"), content).unwrap();

    let a = spans.iter().find(|s| s.name == "a").unwrap();
    assert_eq!(&content[a.body_range.clone()], "{\r\n    b();\r\n}");
    assert_eq!((a.start_line, a.end_line), (1, 3));

    let c = spans.iter().find(|s| s.name == "c").unwrap();
    assert_eq!((c.start_line, c.end_line), (5, 5));
}

#[test]
fn a_file_that_does_not_parse_still_yields_a_tree_and_no_panic() {
    let spans = SpanResolver::new()
        .symbols(Path::new("broken.rs"), "fn broken( {")
        .unwrap();
    assert!(spans.is_empty() || spans.iter().all(|s| s.name == "broken"));
}

#[test]
fn kotlin_is_out_of_scope_and_returns_no_symbols() {
    let spans = SpanResolver::new()
        .symbols(Path::new("x.kt"), "fun a() { }")
        .unwrap();
    assert!(spans.is_empty());
}
```

- [ ] **Step 10: Run them**

Run: `cargo test -p brain-graph span::`
Expected: PASS. If `crlf` fails on `end_line`, the line math is counting `\r` as a line — fix `offset_to_line`, not the test.

- [ ] **Step 11: Commit**

```bash
git add crates/brain-graph/src/span.rs crates/brain-graph/src/lib.rs
git commit -m "feat(graph): add SpanResolver with byte ranges into AST symbols"
```

---

### Task 2: Symbol resolution — qualified names, ambiguity, missing bodies

**Files:**
- Modify: `crates/brain-graph/src/span.rs`
- Test: inline `#[cfg(test)] mod tests` in `span.rs`

**Interfaces:**
- Produces:
  - `pub enum ResolveOutcome { Found(SymbolSpan), Ambiguous(Vec<SymbolSpan>), NotFound { suggestion: Option<String> }, NoBody { candidates: Vec<String> }, NotSupported }`
  - `impl SpanResolver { pub fn resolve(&self, symbols: &[SymbolSpan], symbol: &str) -> ResolveOutcome }`
  - `pub fn render_candidate(span: &SymbolSpan) -> String` — the `impl Foo::new — linha 12` form used in error messages.
- Consumes: everything from Task 1.

- [ ] **Step 1: Write the failing tests**

```rust
fn symbols_of(content: &str) -> Vec<SymbolSpan> {
    SpanResolver::new().symbols(Path::new("x.rs"), content).unwrap()
}

#[test]
fn a_bare_name_found_twice_is_ambiguous_and_lists_both() {
    let content = "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Bar {\n    fn new() -> Self { Bar }\n}\n";
    let spans = symbols_of(content);

    match SpanResolver::new().resolve(&spans, "new") {
        ResolveOutcome::Ambiguous(candidates) => {
            assert_eq!(candidates.len(), 2);
            let rendered: Vec<String> = candidates.iter().map(render_candidate).collect();
            assert!(rendered.iter().any(|c| c.contains("Foo")));
            assert!(rendered.iter().any(|c| c.contains("Bar")));
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn a_qualified_name_picks_the_right_container() {
    let content = "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Bar {\n    fn new() -> Self { Bar }\n}\n";
    let spans = symbols_of(content);

    match SpanResolver::new().resolve(&spans, "Bar::new") {
        ResolveOutcome::Found(span) => {
            assert_eq!(span.container, Some("Bar".to_string()));
            assert_eq!(span.start_line, 6);
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn a_unique_bare_name_resolves_without_qualification() {
    let spans = symbols_of("impl Foo {\n    fn only() {}\n}\n");
    assert!(matches!(
        SpanResolver::new().resolve(&spans, "only"),
        ResolveOutcome::Found(_)
    ));
}

#[test]
fn a_missing_symbol_suggests_the_closest_name() {
    let spans = symbols_of("fn verify_token() {}\nfn logout() {}\n");
    match SpanResolver::new().resolve(&spans, "verfy_token") {
        ResolveOutcome::NotFound { suggestion } => {
            assert_eq!(suggestion, Some("verify_token".to_string()));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn a_trait_fn_without_a_body_is_no_body_not_an_edit() {
    let content = "trait Store {\n    fn get(&self) -> u8;\n}\n";
    let spans = symbols_of(content);
    match SpanResolver::new().resolve(&spans, "get") {
        ResolveOutcome::NoBody { candidates } => assert!(candidates.contains(&"get".to_string())),
        other => panic!("expected NoBody, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p brain-graph span:: --no-run`
Expected: `error[E0433]` — `ResolveOutcome` does not exist.

- [ ] **Step 3: Implement `resolve`, `render_candidate`, and the `NoBody` path**

Resolution order, first match wins:
1. If `symbol` contains `::`, split at the **last** `::` into `container`/`name`; match both.
2. Otherwise match `name` only.
3. Zero matches → `NotFound { suggestion }`, where suggestion is the symbol name with the lowest Levenshtein distance ≤ 3 (implement inline, ~20 lines; no new dependency), `None` if nothing is close.
4. One match → `Found`.
5. More than one → `Ambiguous`.

`NoBody` requires Task 1 to have recorded items it skipped for lacking a body; add a `pub struct Bodyless { pub name: String, pub container: Option<String> }` collector to `symbols` output — simplest change: make `symbols` return `SymbolTable { spans: Vec<SymbolSpan>, bodyless: Vec<Bodyless> }` and update Task 1's tests to `table.spans`. Do that now, in this task, and adjust the earlier tests mechanically.

`render_candidate` formats as `` impl Foo::new — linha 12 `` when `container` is `Some`, `` fn verify_token — linha 12 `` when it is `None`.

- [ ] **Step 4: Run all span tests**

Run: `cargo test -p brain-graph span::`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/brain-graph/src/span.rs
git commit -m "feat(graph): resolve symbols by qualified name, refusing ambiguity"
```

---

### Task 3: `EditTool` — splice, re-parse, atomic write, verify

**Files:**
- Create: `crates/brain-graph/src/edit_tool.rs`
- Modify: `crates/brain-graph/src/lib.rs` (add `pub mod edit_tool;`)
- Test: inline `#[cfg(test)] mod tests` in `edit_tool.rs`

**Interfaces:**
- Produces:
  - `pub struct EditTool` with `pub fn new(root_dir: impl Into<PathBuf>, graph: Arc<RwLock<ProjectCodeGraph>>, store: Arc<ResponseStore>, default_budget_tokens: usize) -> Self`
  - `impl EditTool { pub fn edit(&self, params: EditParams) -> CallToolResult }` (sync; blocking file I/O, called from the async `ToolPlugin::call`)
  - `pub struct EditParams { pub path: String, pub symbol: String, pub expected_hash: String, pub new_body: String, pub signature: Option<String>, pub budget_tokens: Option<usize> }`
  - `pub fn hash_prefix(content: &[u8]) -> String` — 12 hex chars, and `pub fn hash_matches(content: &[u8], expected: &str) -> bool` (accepts full or prefix).
- Consumes: `SpanResolver`, `ResolveOutcome`, `render_candidate` (Tasks 1–2); `budget::select`, `budget::render`, `ResponseStore`.

- [ ] **Step 1: Write the failing test — a successful edit changes only the body**

```rust
fn fixture() -> TempDir {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("auth.rs"),
        "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    !raw.is_empty()\n}\n\npub fn logout() {\n}\n",
    )
    .unwrap();
    dir
}

fn tool(dir: &TempDir) -> EditTool {
    EditTool::new(
        dir.path(),
        Arc::new(RwLock::new(ProjectCodeGraph::new())),
        Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
        4000,
    )
}

#[test]
fn an_edit_replaces_the_body_and_leaves_the_rest_byte_identical() {
    let dir = fixture();
    let before = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let hash = hash_prefix(before.as_bytes());

    let out = tool(&dir).edit(EditParams {
        path: "auth.rs".into(),
        symbol: "verify".into(),
        expected_hash: hash,
        new_body: "{\n    raw.len() > 3\n}".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, None, "{}", out.content[0].text);

    let after = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    assert_eq!(
        after,
        "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    raw.len() > 3\n}\n\npub fn logout() {\n}\n"
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p brain-graph edit_tool:: --no-run`
Expected: `error[E0433]` — `EditTool` does not exist.

- [ ] **Step 3: Implement the happy path**

`edit` runs the pipeline from the spec §3, steps 1–8. Only the parts this test needs are required now: resolve path against `root_dir` (reuse the canonicalize-and-`starts_with` check from `read_tool.rs:90-100`), read as UTF-8, compare hash, resolve span, splice, re-parse with `tree_sitter` (reuse `SupportedLanguage::from_path` + a `Parser`), write to `dir/.<name>.brain-tmp` then `fs::rename`. Return `CallToolResult::text` with the new body read back from disk, the line range, and old/new hash prefixes.

- [ ] **Step 4: Run the test**

Run: `cargo test -p brain-graph edit_tool::an_edit_replaces`
Expected: PASS.

- [ ] **Step 5: Write the failing tests — every refusal path**

```rust
#[test]
fn a_stale_hash_never_writes() {
    let dir = fixture();
    let target = dir.path().join("auth.rs");
    let before = std::fs::read_to_string(&target).unwrap();

    let out = tool(&dir).edit(EditParams {
        path: "auth.rs".into(),
        symbol: "verify".into(),
        expected_hash: "deadbeef0000".into(),
        new_body: "{ true }".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, Some(true));
    assert!(out.content[0].text.contains("Hash atual"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
}

#[test]
fn a_body_that_would_not_parse_never_writes() {
    let dir = fixture();
    let target = dir.path().join("auth.rs");
    let before = std::fs::read_to_string(&target).unwrap();
    let hash = hash_prefix(before.as_bytes());

    let out = tool(&dir).edit(EditParams {
        path: "auth.rs".into(),
        symbol: "verify".into(),
        expected_hash: hash,
        new_body: "{ if raw.len() > }".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, Some(true));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
}

#[test]
fn an_ambiguous_symbol_never_writes_and_lists_candidates() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("x.rs");
    std::fs::write(&target, "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Bar {\n    fn new() -> Self { Bar }\n}\n").unwrap();
    let before = std::fs::read_to_string(&target).unwrap();
    let hash = hash_prefix(before.as_bytes());

    let out = tool(&dir).edit(EditParams {
        path: "x.rs".into(),
        symbol: "new".into(),
        expected_hash: hash,
        new_body: "{ Bar }".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, Some(true));
    assert!(out.content[0].text.contains("Foo"));
    assert!(out.content[0].text.contains("Bar"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
}

#[test]
fn a_path_outside_the_root_is_refused() {
    let dir = fixture();
    let out = tool(&dir).edit(EditParams {
        path: "../../etc/passwd".into(),
        symbol: "root".into(),
        expected_hash: "0000".into(),
        new_body: "{}".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, Some(true));
}

#[test]
fn a_non_utf8_file_is_refused() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("bin.rs"), [0xff, 0xfe, 0x00]).unwrap();
    let out = tool(&dir).edit(EditParams {
        path: "bin.rs".into(),
        symbol: "x".into(),
        expected_hash: "0000".into(),
        new_body: "{}".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, Some(true));
    assert!(out.content[0].text.to_lowercase().contains("utf-8"));
}

#[test]
fn crlf_outside_the_span_survives_the_edit() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("x.rs");
    std::fs::write(&target, "fn a() {\r\n    old();\r\n}\r\n\r\nfn b() {}\r\n").unwrap();
    let before = std::fs::read_to_string(&target).unwrap();
    let hash = hash_prefix(before.as_bytes());

    let out = tool(&dir).edit(EditParams {
        path: "x.rs".into(),
        symbol: "a".into(),
        expected_hash: hash,
        new_body: "{\r\n    new();\r\n}".into(),
        signature: None,
        budget_tokens: None,
    });
    assert_eq!(out.is_error, None, "{}", out.content[0].text);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "fn a() {\r\n    new();\r\n}\r\n\r\nfn b() {}\r\n"
    );
}
```

- [ ] **Step 6: Run them**

Run: `cargo test -p brain-graph edit_tool::`
Expected: the refusal tests fail until each check is implemented.

- [ ] **Step 7: Implement each refusal in pipeline order**

Order matters and is asserted by the tests: path/symlink/UTF-8 → hash → resolve → splice → re-parse → re-hash **immediately before rename** (this is the format-on-save window) → rename → verify. A path whose canonical form differs from the literal join *and* is outside the root is refused as a symlink escape.

- [ ] **Step 8: Run them**

Run: `cargo test -p brain-graph edit_tool::`
Expected: all PASS.

- [ ] **Step 9: Write the failing tests — signature replacement, retry, overflow**

```rust
#[test]
fn a_signature_argument_replaces_the_head_and_keeps_the_body() {
    let dir = fixture();
    let before = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let out = tool(&dir).edit(EditParams {
        path: "auth.rs".into(),
        symbol: "logout".into(),
        expected_hash: hash_prefix(before.as_bytes()),
        new_body: "{ println!(\"bye\"); }".into(),
        signature: Some("pub fn logout(user: &str) ".into()),
        budget_tokens: None,
    });
    assert_eq!(out.is_error, None, "{}", out.content[0].text);

    let after = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    assert!(after.contains("pub fn logout(user: &str) { println!(\"bye\"); }"), "{after}");
    assert!(after.starts_with("use std::io;"));
}

#[test]
fn a_retry_after_a_lost_response_reports_already_applied_and_does_not_rewrite() {
    let dir = fixture();
    let before = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let hash = hash_prefix(before.as_bytes());
    let params = EditParams {
        path: "auth.rs".into(),
        symbol: "verify".into(),
        expected_hash: hash,
        new_body: "{\n    raw.len() > 3\n}".into(),
        signature: None,
        budget_tokens: None,
    };
    assert_eq!(tool(&dir).edit(params.clone()).is_error, None);

    let after_first = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let out = tool(&dir).edit(params); // same stale hash, body already applied
    assert_eq!(out.is_error, None, "{}", out.content[0].text);
    assert!(out.content[0].text.to_lowercase().contains("já aplicada"));
    assert_eq!(std::fs::read_to_string(dir.path().join("auth.rs")).unwrap(), after_first);
}

#[test]
fn a_body_over_budget_is_moved_to_a_handle() {
    let dir = fixture();
    let before = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let big = format!("{{\n{}\n}}", (0..200).map(|i| format!("    let x{i} = {i};")).collect::<Vec<_>>().join("\n"));

    let out = tool(&dir).edit(EditParams {
        path: "auth.rs".into(),
        symbol: "verify".into(),
        expected_hash: hash_prefix(before.as_bytes()),
        new_body: big,
        signature: None,
        budget_tokens: Some(50),
    });
    assert_eq!(out.is_error, None, "{}", out.content[0].text);
    assert!(out.content[0].text.contains("h_"), "{}", out.content[0].text);
}
```

- [ ] **Step 10: Run them**

Run: `cargo test -p brain-graph edit_tool::`
Expected: FAIL until implemented.

- [ ] **Step 11: Implement signature splice, the already-applied check, and the budget rendering**

For the already-applied check: on hash mismatch, resolve the symbol in the **current** content and compare `content[body_range]` with `new_body` after trimming whitespace at both ends. Equal → `já aplicada` text, no write, new hash in the message.

For the budget: build a single `Section` with priority 255 for the header+hash+impact and priority 254 for the body, `budget::select`, then `store.insert(selection.omitted_body())` and `budget::render`.

- [ ] **Step 12: Run them**

Run: `cargo test -p brain-graph edit_tool::`
Expected: all PASS.

- [ ] **Step 13: Write the failing test — the post-write verification catches a mismatched span**

```rust
#[test]
fn the_reported_body_is_the_one_on_disk_not_the_argument() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("x.rs");
    // A body that closes the function early and leaves a stray brace: the file
    // still parses, but the symbol's span is no longer what we think.
    std::fs::write(&target, "fn a() {\n    old();\n}\n").unwrap();
    let before = std::fs::read_to_string(&target).unwrap();

    let out = tool(&dir).edit(EditParams {
        path: "x.rs".into(),
        symbol: "a".into(),
        expected_hash: hash_prefix(before.as_bytes()),
        new_body: "{\n    new();\n}\n}".into(),
        signature: None,
        budget_tokens: None,
    });
    // Either it refuses (brace imbalance detected) or it reports what is on disk.
    let text = &out.content[0].text;
    let disk = std::fs::read_to_string(&target).unwrap();
    assert!(out.is_error == Some(true) || text.contains("new();"));
    assert!(disk.contains("new();") || disk == before, "report must match disk: {disk}");
}
```

- [ ] **Step 14: Run it and confirm the verification is what makes it pass**

Run: `cargo test -p brain-graph edit_tool::the_reported_body_is_the_one_on_disk`
Expected: PASS. Then temporarily replace the post-write read with `new_body` and confirm the test still passes (it asserts either branch) — if it does, tighten the assert to pin one branch and note the chosen behaviour in the commit message.

- [ ] **Step 15: Commit**

```bash
git add crates/brain-graph/src/edit_tool.rs crates/brain-graph/src/lib.rs
git commit -m "feat(graph): add EditTool — hash-guarded AST body splice with post-write verification"
```

---

### Task 4: Impact report in the edit response

**Files:**
- Modify: `crates/brain-graph/src/edit_tool.rs`
- Test: inline `#[cfg(test)] mod tests` in `edit_tool.rs`

**Interfaces:**
- Consumes: `ProjectCodeGraph` (`file_nodes`, `symbol_lookup`, `graph.neighbors_directed`), `DiffTool`'s `is_test` predicate — which is private (`diff_tool.rs:394`). **Promote it**: change `fn is_test` to `pub(crate) fn is_test` in `diff_tool.rs:394` and import it here.
- Produces: the `Impacto:` line in the response.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_response_counts_external_callers_and_reaching_tests() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("core.rs"), "pub fn core_fn() {}\n").unwrap();
    std::fs::write(dir.path().join("app.rs"), "pub fn app() {\n    core_fn();\n}\n").unwrap();
    std::fs::write(dir.path().join("core_test.rs"), "#[test]\nfn test_core() {\n    core_fn();\n}\n").unwrap();

    let graph = crate::builder::GraphBuilder::new().unwrap().build_graph(dir.path()).unwrap();
    let tool = EditTool::new(
        dir.path(),
        Arc::new(RwLock::new(graph)),
        Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
        4000,
    );

    let before = std::fs::read_to_string(dir.path().join("core.rs")).unwrap();
    let out = tool.edit(EditParams {
        path: "core.rs".into(),
        symbol: "core_fn".into(),
        expected_hash: hash_prefix(before.as_bytes()),
        new_body: "{ }".into(),
        signature: None,
        budget_tokens: None,
    });
    let text = &out.content[0].text;
    assert!(text.contains("app.rs"), "external caller missing: {text}");
    assert!(text.contains("test_core"), "reaching test missing: {text}");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p brain-graph edit_tool::the_response_counts`
Expected: FAIL — no `Impacto:` line.

- [ ] **Step 3: Implement the impact line**

Find the node by `file_path == root.join(path)` in `file_nodes`, then `symbol_lookup` for the name, filter `neighbors_directed(Direction::Incoming)` to callers whose `file_path != edited file`. For reaching tests, do the same BFS `DiffTool::affected_tests` does but scoped to this one node — depth 3, `is_test` predicate. Render one line: `` Impacto: N chamador(es) (`a.rs:2`, `b.rs:9`), M teste(s) que alcançam (`core_test.rs:3`) `` — truncate each list at 5 with `…` since the body already fills this section.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p brain-graph`
Expected: all PASS, including the earlier ones (the impact line is additive).

- [ ] **Step 5: Commit**

```bash
git add crates/brain-graph/src/edit_tool.rs crates/brain-graph/src/diff_tool.rs
git commit -m "feat(graph): report callers and reaching tests from brain_edit"
```

---

### Task 5: `brain_read` prints the hash and uses the correct end line

**Files:**
- Modify: `crates/brain-graph/src/read_tool.rs`
- Test: inline `#[cfg(test)] mod tests` in `read_tool.rs`

**Interfaces:**
- Consumes: `hash_prefix` from `edit_tool` (Task 3) — import `crate::edit_tool::hash_prefix`; `SpanResolver` (Task 1).
- Produces: header line `` ### `auth.rs` — 8 linhas · blake3 `a3f1d09c4b22` `` and symbol headings whose end line is the body's end.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_outline_header_carries_the_file_hash() {
    let dir = fixture();
    let tool = tool_for(dir.path());
    let content = std::fs::read_to_string(dir.path().join("auth.rs")).unwrap();
    let expected = hash_prefix(content.as_bytes());

    let out = tool.read(ReadParams {
        path: "auth.rs".into(),
        symbol: None,
        budget_tokens: None,
    });
    assert!(out.content[0].text.contains(&expected), "{}", out.content[0].text);
}

#[test]
fn a_symbol_heading_end_line_is_the_real_end_of_the_body() {
    // The chunker's `end_position().row + 1` reports one line past the brace
    // when a trailing newline is present; the heading must not.
    let dir = fixture();
    let out = tool_for(dir.path()).read(ReadParams {
        path: "auth.rs".into(),
        symbol: Some("verify_token".into()),
        budget_tokens: None,
    });
    assert!(out.content[0].text.contains(":3-5)"), "{}", out.content[0].text);
}
```

- [ ] **Step 2: Run them**

Run: `cargo test -p brain-graph read_tool::`
Expected: FAIL — no hash in the header; heading says `:3-6`.

- [ ] **Step 3: Implement both**

Hash: add it to the outline header section (`read_tool.rs:137`) and to the symbol heading (`read_tool.rs:166`). End line: resolve the symbol via `SpanResolver` and use `span.end_line`; if the resolver does not find it, fall back to the chunker's value rather than dropping the section.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p brain-graph read_tool::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/brain-graph/src/read_tool.rs
git commit -m "feat(graph): print file hash in brain_read and fix symbol end line"
```

---

### Task 6: Register the tool and lock the wire contract

**Files:**
- Modify: `crates/brain-tools/src/main.rs` (register after `ReadTool` block, before `MapTool`)
- Modify: `crates/brain-tools/tests/daemon_smoke_test.rs` (expected names list)
- Test: `crates/brain-tools/tests/brain_edit_e2e_test.rs` (new)

**Interfaces:**
- Consumes: `brain_graph::edit_tool::EditTool`, `brain_graph::span` — both now `pub` from `brain-graph`.
- Produces: `brain_edit` in `tools/list` at position 7.

- [ ] **Step 1: Write the failing test — `tools/list` pins the new name**

In `daemon_smoke_test.rs`, add `"brain_edit"` after `"brain_read"` in the expected `names` vec.

- [ ] **Step 2: Run it**

Run: `cargo test -p brain-tools --test daemon_smoke_test`
Expected: FAIL — expected list has 12 entries, actual has 11.

- [ ] **Step 3: Register the tool**

In `main.rs`, after the `ReadTool` registration:

```rust
registry
    .register(Arc::new(EditTool::new(
        &config.root_dir,
        code_graph.clone(),
        store.clone(),
        config.default_budget_tokens,
    )))
    .map_err(|e| anyhow::anyhow!(e))?;
```

`code_graph` is moved into `HistoryTool::new` later at `main.rs:146`; clone it here. The existing `DiffTool` registration (`main.rs:135`) is the pattern — it takes `code_graph.clone()`. Add `use brain_graph::edit_tool::EditTool;` at the top.

- [ ] **Step 4: Run the smoke test**

Run: `cargo test -p brain-tools --test daemon_smoke_test`
Expected: PASS.

- [ ] **Step 5: Write the failing E2E test — read, edit, read again, check impact**

```rust
// crates/brain-tools/tests/brain_edit_e2e_test.rs
// Spawns the real binary, extracts the hash from brain_read's header, edits with
// it, and asserts brain_read then shows the new body with a different hash.
// Also asserts brain_impact_check on the symbol reflects the edited file.
```

Use the same stdio JSON-RPC shape as `daemon_smoke_test.rs`, but run the child with `--root-dir <tempdir>` so the edit lands in a sandbox. Pass `--exec-disabled` to keep startup fast. Assertions: (a) `brain_read` header contains `blake3`; (b) `brain_edit` succeeds; (c) the second `brain_read` contains the new body and a *different* hash; (d) the file on disk contains the new body and not the old one.

- [ ] **Step 6: Run it**

Run: `cargo test -p brain-tools --test brain_edit_e2e_test`
Expected: FAIL until the test drives the protocol correctly; debug the test harness before touching the tool.

- [ ] **Step 7: Run the whole suite**

Run: `cargo test`
Expected: all PASS. If `brain_search_integration_test` or `graph_tools_test` assert a tool count of 3, they use the legacy dispatcher and stay at 3 — do not change them (see `response_contract_test` precedent in the earlier spec).

- [ ] **Step 8: Commit**

```bash
git add crates/brain-tools/src/main.rs crates/brain-tools/tests/
git commit -m "feat(mcp): register brain_edit and pin it in tools/list"
```

---

### Task 7: Documentation and the agent instruction

**Files:**
- Modify: `README.md` (functionality list; args table if needed; verification step that lists tools)
- Modify: `docs/hooks.md` (the CLAUDE.md block)
- Modify: `docs/guia_de_execucao_e_integracao_mcp.md` (tool mention)

**Interfaces:** none — prose only.

- [ ] **Step 1: Add the README entry**

New numbered item after `brain_read`: what it does, the four required params, that the hash comes from `brain_read`'s header, that it refuses on mismatch/ambiguity/invalid syntax, and that it never formats. Update the "Confirme as ferramentas expostas" list and the `permissions.allow` block to include `mcp__brain-tools__brain_edit`.

- [ ] **Step 2: Update the instruction block in `docs/hooks.md`**

Add one paragraph: editing an already-seen symbol uses `brain_edit` with the hash from `brain_read`; `Read` remains legitimate only for a symbol never seen, for imports, or when a response declared truncation. State plainly that `permissions.deny` rules by path do **not** cover MCP tools, so restricting `brain_edit` needs a deny on the tool name or a `PreToolUse` hook.

- [ ] **Step 3: Verify the docs against the binary**

Run: `cargo run --release -- --help` and compare the tool-adjacent flags quoted in README with the real output. Then run a manual smoke against this repo: `brain_read` on a file, edit a *comment* inside a function via `brain_edit`, confirm with `git diff`, and revert with `git checkout --`.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/hooks.md docs/guia_de_execucao_e_integracao_mcp.md
git commit -m "docs: document brain_edit and when the agent may skip Read"
```

---

## Self-Review Notes

- **Spec coverage:** interface (§1) → Tasks 3/6; span resolver (§2) → Tasks 1/2; pipeline (§3) → Task 3; concurrency/retry (§4) → Task 3 Step 11; response (§5) → Tasks 3/4; `brain_read` changes (§6) → Task 5; instruction (§7) → Task 7. Non-goals are honoured: no Kotlin (Task 1 returns empty for `.kt`), no formatting call, no annotations field (explicitly excluded in Global Constraints).
- **Deliberate deviation from the spec:** the spec placed the impact summary inside the success response only. Task 3 keeps the response minimal and Task 4 adds impact, so a reviewer can reject the impact scoring without reopening the write path.
- **`end_line` fix scope:** the spec said `brain_read` only. `brain_search` keeps the chunker's optimistic `end_line`; changing it would alter existing search output and belongs to another spec.
