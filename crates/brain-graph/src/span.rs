use brain_kernel::ast::SupportedLanguage;
use brain_kernel::{BrainError, Result};
use std::ops::Range;
use std::path::Path;
use tree_sitter::{Node, Parser};

/// A named item in a source file, with byte ranges instead of line ranges.
///
/// `AstSnippet` is built for reading: it carries line numbers and a truncated
/// body. An edit needs exact byte offsets and the whole body, otherwise a
/// splice can silently land one line off or cut a long function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSpan {
    pub name: String,
    pub container: Option<String>,
    pub kind: String,
    /// Bytes from the start of the item to the `{` of the body.
    pub head_range: Range<usize>,
    /// Bytes from the `{` of the body to the `}` inclusive.
    pub body_range: Range<usize>,
    /// 1-based, derived from `body_range`.
    pub start_line: usize,
    /// 1-based, derived from `body_range`.
    pub end_line: usize,
}

/// An item that has a name but no `{ … }` body (`fn get(&self);`, `struct U;`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bodyless {
    pub name: String,
    pub container: Option<String>,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolTable {
    pub spans: Vec<SymbolSpan>,
    pub bodyless: Vec<Bodyless>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Found(SymbolSpan),
    Ambiguous(Vec<SymbolSpan>),
    NotFound { suggestion: Option<String> },
    NoBody { candidates: Vec<String> },
    NotSupported,
}

/// Resolves symbols to byte ranges using the same tree-sitter grammars as the
/// chunker, but descending into containers so a method is not returned as its
/// whole `impl`.
#[derive(Debug, Default)]
pub struct SpanResolver;

impl SpanResolver {
    pub fn new() -> Self {
        Self
    }

    pub fn table(&self, path: &Path, content: &str) -> Result<SymbolTable> {
        match SupportedLanguage::from_path(path) {
            Some(SupportedLanguage::Rust) => {}
            // Kotlin edits are out of scope until the body rules are exercised.
            Some(SupportedLanguage::Kotlin) | None => return Ok(SymbolTable::default()),
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| BrainError::Ast(format!("Rust grammar load error: {e}")))?;
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| BrainError::Ast("Failed to parse AST".to_string()))?;

        let mut table = SymbolTable::default();
        collect_children(tree.root_node(), content, &mut table, None);
        Ok(table)
    }

    pub fn symbols(&self, path: &Path, content: &str) -> Result<Vec<SymbolSpan>> {
        Ok(self.table(path, content)?.spans)
    }

    /// Resolves a bare (`verify`) or qualified (`Auth::verify`) name.
    ///
    /// Ambiguity is never resolved by picking the first candidate: a wrong
    /// edit is worse than an extra round trip.
    pub fn resolve(&self, table: &SymbolTable, symbol: &str) -> ResolveOutcome {
        let (container, name) = split_qualified(symbol);

        let matching: Vec<SymbolSpan> = table
            .spans
            .iter()
            .filter(|span| {
                span.name == name && container_matches(span.container.as_deref(), container)
            })
            .cloned()
            .collect();

        match matching.len() {
            1 => return ResolveOutcome::Found(matching.into_iter().next().unwrap()),
            n if n > 1 => return ResolveOutcome::Ambiguous(matching),
            _ => {}
        }

        let bodyless: Vec<String> = table
            .bodyless
            .iter()
            .filter(|item| {
                item.name == name && container_matches(item.container.as_deref(), container)
            })
            .map(render_bodyless)
            .collect();
        if !bodyless.is_empty() {
            return ResolveOutcome::NoBody {
                candidates: bodyless,
            };
        }

        let suggestion = table
            .spans
            .iter()
            .map(|span| span.name.clone())
            .chain(table.bodyless.iter().map(|item| item.name.clone()))
            .map(|candidate| {
                let distance = levenshtein(&candidate, name);
                (candidate, distance)
            })
            .filter(|(_, distance)| *distance <= 3)
            .min_by_key(|(_, distance)| *distance)
            .map(|(candidate, _)| candidate);

        ResolveOutcome::NotFound { suggestion }
    }
}

/// `impl Foo::new — linha 12`, or `fn verify — linha 3`.
pub fn render_candidate(span: &SymbolSpan) -> String {
    match &span.container {
        Some(container) => format!(
            "impl {container}::{} — linha {}",
            span.name, span.start_line
        ),
        None => format!("{} {} — linha {}", span.kind, span.name, span.start_line),
    }
}

fn render_bodyless(item: &Bodyless) -> String {
    match &item.container {
        Some(container) => format!("{container}::{} — linha {}", item.name, item.line),
        None => format!("{} — linha {}", item.name, item.line),
    }
}

/// 1-based line for a byte offset, `\r\n` safe: `\r` is never counted.
pub fn offset_to_line(content: &str, offset: usize) -> usize {
    let offset = offset.min(content.len());
    content.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn split_qualified(symbol: &str) -> (Option<&str>, &str) {
    match symbol.rsplit_once("::") {
        Some((container, name)) => (Some(container.trim()), name.trim()),
        None => (None, symbol.trim()),
    }
}

fn container_matches(actual: Option<&str>, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    let requested = requested
        .strip_prefix("impl ")
        .or_else(|| requested.strip_prefix("trait "))
        .or_else(|| requested.strip_prefix("mod "))
        .unwrap_or(requested);
    let Some(actual) = actual else {
        return false;
    };
    actual == requested || format!("impl {actual}") == requested
}

fn collect_children(node: Node, content: &str, table: &mut SymbolTable, container: Option<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        let is_symbol = matches!(
            kind,
            "function_item"
                | "function_signature_item"
                | "impl_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "mod_item"
        );
        let is_container = matches!(kind, "impl_item" | "trait_item" | "mod_item");
        let name = item_name(child, content);

        if is_symbol {
            match body_node(child) {
                Some(body) => {
                    let head_range = child.start_byte()..body.start_byte();
                    let body_range = body.start_byte()..body.end_byte();
                    table.spans.push(SymbolSpan {
                        name: name.clone(),
                        container: container.clone(),
                        kind: display_kind(kind),
                        start_line: offset_to_line(content, body_range.start),
                        end_line: offset_to_line(content, body_range.end.saturating_sub(1)),
                        head_range,
                        body_range,
                    });
                }
                None => table.bodyless.push(Bodyless {
                    name: name.clone(),
                    container: container.clone(),
                    kind: display_kind(kind),
                    line: child.start_position().row + 1,
                }),
            }
        }

        let next_container = if is_container {
            container_name(child, content).or(Some(name))
        } else {
            container.clone()
        };
        // Descend through syntax wrappers (`source_file`, `declaration_list`,
        // `ERROR`) and through containers, but not into a function body: nested
        // items are not what `symbol` names.
        if is_container || !is_symbol {
            collect_children(child, content, table, next_container);
        }
    }
}

/// The node holding an item's body, whatever the grammar calls it.
///
/// `child_by_field_name("body")` covers functions, impls, traits and modules.
/// Structs and enums name the list differently across grammar versions, so the
/// fallback accepts their body kinds by name.
fn body_node(node: Node) -> Option<Node> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| {
            matches!(
                child.kind(),
                "block" | "declaration_list" | "field_declaration_list" | "enum_variant_list"
            )
        })
        .last()
}

/// The name that qualifies an item's children.
///
/// A trait impl is qualified by the trait, not the self type: `impl Foo { fn
/// new() }` and `impl Default for Foo { fn new() }` can coexist, and both
/// rendered as `Foo::new` would be an ambiguity with no addressable candidate.
fn container_name(node: Node, content: &str) -> Option<String> {
    let field = match node.kind() {
        "impl_item" if node.child_by_field_name("trait").is_some() => "trait",
        "impl_item" => "type",
        _ => "name",
    };
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(content.as_bytes()).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn item_name(node: Node, content: &str) -> String {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(content.as_bytes()).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    {
        return name;
    }
    if node.kind() == "impl_item" {
        if let Some(ty) = node
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(content.as_bytes()).ok())
        {
            return format!("impl {}", ty.trim());
        }
    }
    display_kind(node.kind())
}

fn display_kind(kind: &str) -> String {
    match kind {
        "function_item" | "function_signature_item" => "fn".to_string(),
        "impl_item" => "impl".to_string(),
        "struct_item" => "struct".to_string(),
        "enum_item" => "enum".to_string(),
        "trait_item" => "trait".to_string(),
        "mod_item" => "mod".to_string(),
        other => other.to_string(),
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols_of(content: &str) -> SymbolTable {
        SpanResolver::new()
            .table(Path::new("x.rs"), content)
            .unwrap()
    }

    fn span<'a>(table: &'a SymbolTable, name: &str) -> &'a SymbolSpan {
        table
            .spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}: {:?}", table.spans))
    }

    #[test]
    fn a_top_level_function_body_range_covers_exactly_the_braces() {
        let content =
            "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    !raw.is_empty()\n}\n";
        let table = symbols_of(content);
        let span = span(&table, "verify");

        assert_eq!(
            &content[span.body_range.clone()],
            "{\n    !raw.is_empty()\n}"
        );
        assert!(content[span.head_range.clone()].ends_with("bool "));
        assert_eq!(span.start_line, 3);
        assert_eq!(span.end_line, 5);
        assert_eq!(span.container, None);
    }

    #[test]
    fn a_method_inside_an_impl_resolves_to_the_method() {
        let content = "impl Auth {\n    pub fn verify(&self) -> bool {\n        true\n    }\n\n    pub fn logout(&self) {}\n}\n";
        let table = symbols_of(content);

        let verify = span(&table, "verify");
        assert_eq!(
            &content[verify.body_range.clone()],
            "{\n        true\n    }"
        );
        assert_eq!(verify.container.as_deref(), Some("Auth"));

        let logout = span(&table, "logout");
        assert_eq!(&content[logout.body_range.clone()], "{}");
        assert!(table
            .spans
            .iter()
            .any(|span| span.name == "impl Auth" && span.container.is_none()));
    }

    #[test]
    fn struct_and_enum_bodies_are_editable() {
        let content = "struct Point {\n    x: u8,\n}\n\nenum Color {\n    Red,\n}\n";
        let table = symbols_of(content);

        assert_eq!(
            &content[span(&table, "Point").body_range.clone()],
            "{\n    x: u8,\n}"
        );
        assert_eq!(
            &content[span(&table, "Color").body_range.clone()],
            "{\n    Red,\n}"
        );
    }

    #[test]
    fn a_trait_impl_is_qualified_by_the_trait_not_the_type() {
        let content = "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Default for Foo {\n    fn new() -> Self { Foo }\n}\n";
        let table = symbols_of(content);

        match SpanResolver::new().resolve(&table, "Default::new") {
            ResolveOutcome::Found(span) => assert_eq!(span.start_line, 6),
            other => panic!("expected Found, got {other:?}"),
        }
        match SpanResolver::new().resolve(&table, "new") {
            ResolveOutcome::Ambiguous(candidates) => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn crlf_content_keeps_byte_offsets_and_reports_lines_without_counting_cr() {
        let content = "fn a() {\r\n    b();\r\n}\r\n\r\nfn c() {}\r\n";
        let table = symbols_of(content);

        let a = span(&table, "a");
        assert_eq!(&content[a.body_range.clone()], "{\r\n    b();\r\n}");
        assert_eq!((a.start_line, a.end_line), (1, 3));

        let c = span(&table, "c");
        assert_eq!((c.start_line, c.end_line), (5, 5));
    }

    #[test]
    fn a_file_that_does_not_parse_still_yields_a_tree_and_no_panic() {
        let table = symbols_of("fn broken( {");
        assert!(table.spans.is_empty() || table.spans.iter().all(|span| span.name == "broken"));
    }

    #[test]
    fn kotlin_is_out_of_scope_and_returns_no_symbols() {
        let table = SpanResolver::new()
            .table(Path::new("x.kt"), "fun a() { }")
            .unwrap();
        assert!(table.spans.is_empty());
    }

    #[test]
    fn a_bare_name_found_twice_is_ambiguous_and_lists_both() {
        let content = "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Bar {\n    fn new() -> Self { Bar }\n}\n";
        let table = symbols_of(content);

        match SpanResolver::new().resolve(&table, "new") {
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
        let table = symbols_of(content);

        match SpanResolver::new().resolve(&table, "Bar::new") {
            ResolveOutcome::Found(span) => {
                assert_eq!(span.container.as_deref(), Some("Bar"));
                assert_eq!(span.start_line, 6);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn a_unique_bare_name_resolves_without_qualification() {
        let table = symbols_of("impl Foo {\n    fn only() {}\n}\n");
        assert!(matches!(
            SpanResolver::new().resolve(&table, "only"),
            ResolveOutcome::Found(_)
        ));
    }

    #[test]
    fn a_missing_symbol_suggests_the_closest_name() {
        let table = symbols_of("fn verify_token() {}\nfn logout() {}\n");
        match SpanResolver::new().resolve(&table, "verfy_token") {
            ResolveOutcome::NotFound { suggestion } => {
                assert_eq!(suggestion.as_deref(), Some("verify_token"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_trait_fn_without_a_body_is_no_body_not_an_edit() {
        let content = "trait Store {\n    fn get(&self) -> u8;\n}\n";
        let table = symbols_of(content);
        match SpanResolver::new().resolve(&table, "get") {
            ResolveOutcome::NoBody { candidates } => {
                assert!(candidates.iter().any(|c| c.contains("get")))
            }
            other => panic!("expected NoBody, got {other:?}"),
        }
    }
}
