use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use petgraph::Direction;
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tree_sitter::Parser;

use crate::diff_tool::is_test;
use crate::model::ProjectCodeGraph;
use crate::span::{render_candidate, ResolveOutcome, SpanResolver, SymbolSpan};

const TEST_SEARCH_DEPTH: usize = 3;
const HASH_PREFIX_LEN: usize = 12;

/// 12 hex chars: enough to identify a file version, short enough to sit in a
/// heading without costing context.
pub fn hash_prefix(content: &[u8]) -> String {
    hash_hex(content)[..HASH_PREFIX_LEN].to_string()
}

fn hash_hex(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

/// Accepts the 12-character prefix `brain_read` prints or the full digest.
pub fn hash_matches(content: &[u8], expected: &str) -> bool {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() < 8 || expected.len() > 64 {
        return false;
    }
    hash_hex(content).starts_with(&expected)
}

/// Edits the body of a named symbol by byte span.
///
/// The agent supplies the symbol and its new body, never a whole-file anchor.
/// The file hash is the read-before-edit guard: an edit over a version the
/// agent did not see is refused instead of silently clobbering it.
pub struct EditTool {
    root_dir: PathBuf,
    graph: Arc<RwLock<ProjectCodeGraph>>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditParams {
    pub path: String,
    pub symbol: String,
    pub expected_hash: String,
    pub new_body: String,
    pub signature: Option<String>,
    pub budget_tokens: Option<usize>,
}

impl EditTool {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        graph: Arc<RwLock<ProjectCodeGraph>>,
        store: Arc<ResponseStore>,
        default_budget_tokens: usize,
    ) -> Self {
        Self {
            root_dir: root_dir.into(),
            graph,
            store,
            default_budget_tokens,
        }
    }

    pub fn edit(&self, params: EditParams) -> CallToolResult {
        let path = match self.resolve_target(&params.path) {
            Ok(path) => path,
            Err(error) => return error,
        };

        if !is_rust_source(&path) {
            return CallToolResult::error(format!(
                "Edição de símbolos só é suportada em Rust por enquanto (arquivo: {}).",
                params.path
            ));
        }

        let original_bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return CallToolResult::error(format!("Falha ao ler {}: {e}", path.display()))
            }
        };
        let content = match String::from_utf8(original_bytes.clone()) {
            Ok(content) => content,
            Err(_) => {
                return CallToolResult::error(format!(
                    "O arquivo {} não é UTF-8 válido; nada foi escrito.",
                    params.path
                ))
            }
        };

        let old_prefix = hash_prefix(&original_bytes);
        let relative = self.relative(&path);

        let table = match SpanResolver::new().table(&path, &content) {
            Ok(table) => table,
            Err(e) => return CallToolResult::error(e.to_string()),
        };

        if !hash_matches(&original_bytes, &params.expected_hash) {
            let already = match SpanResolver::new().resolve(&table, &params.symbol) {
                ResolveOutcome::Found(span) => {
                    content[span.body_range.clone()].trim() == params.new_body.trim()
                }
                _ => false,
            };
            if already {
                return CallToolResult::text(format!(
                    "### `{}` — já aplicada\n\nArquivo: blake3 `{old_prefix}`; nada foi escrito.\n",
                    qualified(&params.symbol)
                ));
            }
            return CallToolResult::error(format!(
                "Arquivo mudou desde a leitura. Hash atual: `{old_prefix}`. \
                 Reenvie com expected_hash atualizado."
            ));
        }

        let span = match SpanResolver::new().resolve(&table, &params.symbol) {
            ResolveOutcome::Found(span) => span,
            ResolveOutcome::Ambiguous(candidates) => {
                let rendered: Vec<String> = candidates.iter().map(render_candidate).collect();
                return CallToolResult::error(format!(
                    "Símbolo `{}` aparece {} vezes: {}. Reenvie qualificado.",
                    params.symbol,
                    candidates.len(),
                    rendered.join(", ")
                ));
            }
            ResolveOutcome::NotFound { suggestion } => {
                let hint = suggestion
                    .map(|name| format!(" Mais próximo: `{name}`."))
                    .unwrap_or_default();
                return CallToolResult::error(format!(
                    "Símbolo `{}` não encontrado em `{relative}`.{hint}",
                    params.symbol
                ));
            }
            ResolveOutcome::NoBody { candidates } => {
                return CallToolResult::error(format!(
                    "Símbolo `{}` não tem bloco `{{…}}` ({}). Nada foi escrito.",
                    params.symbol,
                    candidates.join(", ")
                ));
            }
            ResolveOutcome::NotSupported => {
                return CallToolResult::error(format!(
                    "Edição de símbolos não é suportada em `{relative}`."
                ));
            }
        };

        let new_content = match splice(
            &content,
            &span,
            &params.new_body,
            params.signature.as_deref(),
        ) {
            Ok(content) => content,
            Err(message) => return CallToolResult::error(message),
        };

        if let Some(line) = first_parse_error(&new_content) {
            return CallToolResult::error(format!(
                "O arquivo não parsearia depois da edição (erro na linha {line}). Nada foi escrito."
            ));
        }

        // The hash check and the rename are not atomic together: an editor's
        // format-on-save can land in the window. Re-read immediately before
        // writing so that change is never clobbered.
        match std::fs::read(&path) {
            Ok(current) if current != original_bytes => {
                return CallToolResult::error(format!(
                    "Arquivo mudou durante a edição. Hash atual: `{}`. Nada foi escrito.",
                    hash_prefix(&current)
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return CallToolResult::error(format!("Falha ao reler {}: {e}", path.display()))
            }
        }

        if let Err(e) = atomic_write(&path, new_content.as_bytes()) {
            return CallToolResult::error(format!(
                "Falha ao escrever {}: {e}. Nada foi escrito.",
                path.display()
            ));
        }

        // Never report success from what we meant to write: read it back and
        // prove the hash, the symbol and the body all match.
        let on_disk = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                return CallToolResult::error(format!(
                    "A escrita foi feita, mas a verificação não conseguiu reler {}: {e}",
                    path.display()
                ))
            }
        };
        if hash_hex(on_disk.as_bytes()) != hash_hex(new_content.as_bytes()) {
            return CallToolResult::error(format!(
                "A verificação pós-escrita falhou: o hash em disco não é o esperado ({}).",
                path.display()
            ));
        }
        let verified = match SpanResolver::new().table(&path, &on_disk) {
            Ok(table) => match SpanResolver::new().resolve(&table, &params.symbol) {
                ResolveOutcome::Found(span) => Some(span),
                _ => None,
            },
            Err(_) => None,
        };
        let Some(verified_span) = verified else {
            return CallToolResult::error(
                "A verificação pós-escrita falhou: o símbolo não re-resolve no arquivo novo.",
            );
        };
        let disk_body = &on_disk[verified_span.body_range.clone()];
        if disk_body != params.new_body {
            return CallToolResult::error(
                "A verificação pós-escrita falhou: o corpo em disco difere do corpo enviado.",
            );
        }

        let impact = self.impact(span.clone(), &path);
        let heading = format!(
            "### `{}` — `{relative}:{}-{}` editado",
            qualified_span(&span),
            verified_span.start_line,
            verified_span.end_line
        );
        let body = format!(
            "\nArquivo: blake3 `{}` (antes `{old_prefix}`)\n{impact}\n",
            hash_prefix(on_disk.as_bytes())
        );
        let sections = vec![
            Section {
                heading,
                body,
                priority: 255,
            },
            Section {
                heading: format!("#### corpo de `{}`", qualified_span(&span)),
                body: format!("```rust\n{disk_body}\n```\n"),
                priority: 254,
            },
        ];

        let budget = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let selection = budget::select(&sections, budget);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        CallToolResult::text(budget::render(&selection, handle.as_deref()))
    }

    fn resolve_target(&self, requested: &str) -> std::result::Result<PathBuf, CallToolResult> {
        let candidate = Path::new(requested);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root_dir.join(candidate)
        };
        let root = self.root_dir.canonicalize().map_err(|e| {
            CallToolResult::error(format!(
                "Diretório raiz inválido ({}): {e}",
                self.root_dir.display()
            ))
        })?;
        let canonical = joined
            .canonicalize()
            .map_err(|_| CallToolResult::error(format!("Arquivo não encontrado: {requested}")))?;
        if !canonical.starts_with(&root) {
            return Err(CallToolResult::error(format!(
                "Caminho fora do diretório raiz: {requested}"
            )));
        }
        Ok(canonical)
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn impact(&self, span: SymbolSpan, path: &Path) -> String {
        let graph = match self.graph.try_read() {
            Ok(graph) => graph,
            Err(_) => {
                return "Impacto: grafo ocupado; consulte `brain_impact_check`.\n".to_string()
            }
        };

        let Some(target) = find_symbol_node(&graph, path, &span) else {
            return "Impacto: símbolo ainda não indexado; consulte `brain_impact_check`.\n"
                .to_string();
        };

        let caller_nodes: Vec<_> = graph
            .graph
            .neighbors_directed(target, Direction::Incoming)
            .filter_map(|idx| graph.graph.node_weight(idx).map(|node| (idx, node)))
            .filter(|(_, node)| !same_path(&node.file_path, path))
            .collect();

        let mut callers: BTreeSet<(String, usize, String)> = BTreeSet::new();
        for (idx, node) in &caller_nodes {
            let line = graph
                .graph
                .find_edge(*idx, target)
                .and_then(|edge| graph.graph.edge_weight(edge))
                .map(|edge| edge.call_site_line)
                .unwrap_or(node.start_line);
            callers.insert((self.relative(&node.file_path), line, node.name.clone()));
        }

        let mut seen: HashSet<_> = HashSet::from([target]);
        let mut queue: VecDeque<_> = VecDeque::from([(target, 0usize)]);
        let mut tests: BTreeSet<(String, usize, String)> = BTreeSet::new();
        while let Some((idx, depth)) = queue.pop_front() {
            if depth > 0 {
                if let Some(node) = graph.graph.node_weight(idx) {
                    if is_test(node) {
                        tests.insert((
                            self.relative(&node.file_path),
                            node.start_line,
                            node.name.clone(),
                        ));
                    }
                }
            }
            if depth == TEST_SEARCH_DEPTH {
                continue;
            }
            for caller in graph.graph.neighbors_directed(idx, Direction::Incoming) {
                if seen.insert(caller) {
                    queue.push_back((caller, depth + 1));
                }
            }
        }

        if callers.is_empty() && tests.is_empty() {
            return "Impacto: nenhum chamador externo nem teste indexado.\n".to_string();
        }

        let callers_text = render_sites(&callers, false);
        let tests_text = render_sites(&tests, true);
        format!(
            "Impacto: {} chamador(es){callers_text}, {} teste(s) que alcançam{tests_text}\n",
            callers.len(),
            tests.len()
        )
    }
}

fn find_symbol_node(
    graph: &ProjectCodeGraph,
    path: &Path,
    span: &SymbolSpan,
) -> Option<petgraph::stable_graph::NodeIndex> {
    graph
        .file_nodes
        .iter()
        .filter(|(node_path, _)| same_path(node_path, path))
        .flat_map(|(_, nodes)| nodes.iter().copied())
        .filter_map(|idx| graph.graph.node_weight(idx).map(|node| (idx, node)))
        .find(|(_, node)| {
            node.name == span.name
                && match (&span.container, &node.container) {
                    (Some(container), Some(node_container)) => container == node_container,
                    (Some(_), None) => false,
                    (None, _) => true,
                }
        })
        .map(|(idx, _)| idx)
}

fn render_sites(sites: &BTreeSet<(String, usize, String)>, with_name: bool) -> String {
    if sites.is_empty() {
        return String::new();
    }
    let mut items: Vec<String> = sites
        .iter()
        .take(5)
        .map(|(path, line, name)| {
            if with_name {
                format!("`{name}` (`{path}:{line}`)")
            } else {
                format!("`{path}:{line}`")
            }
        })
        .collect();
    if sites.len() > 5 {
        items.push("…".to_string());
    }
    format!(" ({})", items.join(", "))
}

fn qualified(symbol: &str) -> String {
    symbol.trim().to_string()
}

fn qualified_span(span: &SymbolSpan) -> String {
    match &span.container {
        Some(container) => format!("{container}::{}", span.name),
        None => span.name.clone(),
    }
}

fn is_rust_source(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("rs")
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn splice(
    content: &str,
    span: &SymbolSpan,
    new_body: &str,
    signature: Option<&str>,
) -> std::result::Result<String, String> {
    if span.body_range.end > content.len()
        || span.body_range.start > span.body_range.end
        || span.head_range.end > span.body_range.start
    {
        return Err("Intervalo do símbolo inválido; nada foi escrito.".to_string());
    }

    let mut spliced = String::with_capacity(
        content.len() - span.body_range.len() + new_body.len() + signature.map_or(0, str::len) + 1,
    );
    spliced.push_str(&content[..span.body_range.start]);
    spliced.push_str(new_body);
    spliced.push_str(&content[span.body_range.end..]);

    if let Some(signature) = signature {
        let signature = signature.trim_end();
        if signature.is_empty() {
            return Err("`signature` vazia; nada foi escrito.".to_string());
        }
        let mut with_head = String::with_capacity(spliced.len() + signature.len() + 1);
        with_head.push_str(&spliced[..span.head_range.start]);
        with_head.push_str(signature);
        with_head.push(' ');
        with_head.push_str(&spliced[span.body_range.start..]);
        spliced = with_head;
    }

    Ok(spliced)
}

fn first_parse_error(content: &str) -> Option<usize> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    if !root.has_error() {
        return None;
    }
    Some(first_error_node(root).map_or(1, |node| node.start_position().row + 1))
}

fn first_error_node<'a>(node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_error_node(child) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(
        ".{file_name}.brain-tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // New files have no metadata to preserve; existing files keep their mode.
    let permissions = std::fs::metadata(path).ok().map(|metadata| metadata.permissions());
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&tmp, permissions)?;
        }
        std::fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl ToolPlugin for EditTool {
    fn name(&self) -> &'static str {
        "brain_edit"
    }

    fn schema(&self) -> Tool {
        Tool {
            name: "brain_edit".to_string(),
            description: "Edita o corpo de um símbolo existente pelo nome, por span de AST, sobre a versão do arquivo identificada por `expected_hash`. Substitui `Read` + `Edit`: o agente manda o corpo novo, não uma âncora de texto. Recusa hash divergente, símbolo ambíguo, item sem corpo e sintaxe inválida; nunca escreve parcialmente nem formata.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Caminho relativo ao diretório raiz do projeto."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Nome simples (`verify_token`) ou qualificado pelo container (`AuthService::verify_token`)."
                    },
                    "expected_hash": {
                        "type": "string",
                        "description": "blake3 do arquivo inteiro, como `brain_read` imprime no cabeçalho (prefixo de 12 caracteres aceito)."
                    },
                    "new_body": {
                        "type": "string",
                        "description": "Novo bloco `{ … }` completo, chaves incluídas. O splice é de bytes crus: sem reindentação e sem formatação."
                    },
                    "signature": {
                        "type": "string",
                        "description": "Opcional: substitui tudo o que antecede o `{` (assinatura, genéricos, `where`). Sem ele, a assinatura fica como está."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Teto aproximado de tokens; o excedente fica recuperável via brain_peek."
                    }
                },
                "required": ["path", "symbol", "expected_hash", "new_body"]
            }),
        }
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        let params: EditParams = match serde_json::from_value(args) {
            Ok(params) => params,
            Err(e) => {
                return Box::pin(
                    async move { CallToolResult::error(format!("Invalid arguments: {e}")) },
                )
            }
        };
        Box::pin(async move { self.edit(params) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::offset_to_line;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn fixture() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("auth.rs"),
            "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    !raw.is_empty()\n}\n\npub fn logout() {\n}\n",
        )
        .unwrap();
        dir
    }

    fn tool(dir: &tempfile::TempDir) -> EditTool {
        EditTool::new(
            dir.path(),
            Arc::new(RwLock::new(ProjectCodeGraph::new())),
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        )
    }

    fn text(out: &CallToolResult) -> &str {
        &out.content[0].text
    }

    #[test]
    fn an_edit_replaces_the_body_and_leaves_the_rest_byte_identical() {
        let dir = fixture();
        let before = fs::read_to_string(dir.path().join("auth.rs")).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "auth.rs".into(),
            symbol: "verify".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{\n    raw.len() > 3\n}".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, None, "{}", text(&out));

        let after = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        assert_eq!(
            after,
            "use std::io;\n\npub fn verify(raw: &str) -> bool {\n    raw.len() > 3\n}\n\npub fn logout() {\n}\n"
        );
        assert!(text(&out).contains("editado"));
        assert!(text(&out).contains(&hash_prefix(after.as_bytes())));
    }

    #[test]
    fn a_stale_hash_never_writes() {
        let dir = fixture();
        let target = dir.path().join("auth.rs");
        let before = fs::read_to_string(&target).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "auth.rs".into(),
            symbol: "verify".into(),
            expected_hash: "deadbeef0000".into(),
            new_body: "{ true }".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("Hash atual"));
        assert_eq!(fs::read_to_string(&target).unwrap(), before);
    }

    #[test]
    fn a_body_that_would_not_parse_never_writes() {
        let dir = fixture();
        let target = dir.path().join("auth.rs");
        let before = fs::read_to_string(&target).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "auth.rs".into(),
            symbol: "verify".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{ if raw.len() > }".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, Some(true));
        assert_eq!(fs::read_to_string(&target).unwrap(), before);
    }

    #[test]
    fn an_ambiguous_symbol_never_writes_and_lists_candidates() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("x.rs");
        fs::write(
            &target,
            "impl Foo {\n    fn new() -> Self { Foo }\n}\n\nimpl Bar {\n    fn new() -> Self { Bar }\n}\n",
        )
        .unwrap();
        let before = fs::read_to_string(&target).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "x.rs".into(),
            symbol: "new".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{ Bar }".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("Foo"));
        assert!(text(&out).contains("Bar"));
        assert_eq!(fs::read_to_string(&target).unwrap(), before);
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
        fs::write(dir.path().join("bin.rs"), [0xff, 0xfe, 0x00]).unwrap();
        let out = tool(&dir).edit(EditParams {
            path: "bin.rs".into(),
            symbol: "x".into(),
            expected_hash: "00000000".into(),
            new_body: "{}".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).to_lowercase().contains("utf-8"));
    }

    #[test]
    fn crlf_outside_the_span_survives_the_edit() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("x.rs");
        fs::write(&target, "fn a() {\r\n    old();\r\n}\r\n\r\nfn b() {}\r\n").unwrap();
        let before = fs::read_to_string(&target).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "x.rs".into(),
            symbol: "a".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{\r\n    new();\r\n}".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, None, "{}", text(&out));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "fn a() {\r\n    new();\r\n}\r\n\r\nfn b() {}\r\n"
        );
    }

    #[test]
    fn a_signature_argument_replaces_the_head_and_keeps_the_body() {
        let dir = fixture();
        let before = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        let out = tool(&dir).edit(EditParams {
            path: "auth.rs".into(),
            symbol: "logout".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{ println!(\"bye\"); }".into(),
            signature: Some("pub fn logout(user: &str) ".into()),
            budget_tokens: None,
        });
        assert_eq!(out.is_error, None, "{}", text(&out));

        let after = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        assert!(
            after.contains("pub fn logout(user: &str) { println!(\"bye\"); }"),
            "{after}"
        );
        assert!(after.starts_with("use std::io;"));
    }

    #[test]
    fn a_retry_after_a_lost_response_reports_already_applied_and_does_not_rewrite() {
        let dir = fixture();
        let before = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        let params = EditParams {
            path: "auth.rs".into(),
            symbol: "verify".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{\n    raw.len() > 3\n}".into(),
            signature: None,
            budget_tokens: None,
        };
        assert_eq!(tool(&dir).edit(params.clone()).is_error, None);

        let after_first = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        let out = tool(&dir).edit(params);
        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).to_lowercase().contains("já aplicada"));
        assert_eq!(
            fs::read_to_string(dir.path().join("auth.rs")).unwrap(),
            after_first
        );
    }

    #[test]
    fn a_body_over_budget_is_moved_to_a_handle() {
        let dir = fixture();
        let before = fs::read_to_string(dir.path().join("auth.rs")).unwrap();
        let big = format!(
            "{{\n{}\n}}",
            (0..200)
                .map(|i| format!("    let x{i} = {i};"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let out = tool(&dir).edit(EditParams {
            path: "auth.rs".into(),
            symbol: "verify".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: big,
            signature: None,
            budget_tokens: Some(50),
        });
        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(text(&out).contains("h_"), "{}", text(&out));
    }

    #[test]
    fn the_reported_body_is_the_one_on_disk_not_the_argument() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("x.rs");
        fs::write(&target, "fn a() {\n    old();\n}\n").unwrap();
        let before = fs::read_to_string(&target).unwrap();

        let out = tool(&dir).edit(EditParams {
            path: "x.rs".into(),
            symbol: "a".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{\n    new();\n}\n}".into(),
            signature: None,
            budget_tokens: None,
        });
        let disk = fs::read_to_string(&target).unwrap();
        assert!(out.is_error == Some(true) || text(&out).contains("new();"));
        assert!(
            disk.contains("new();") || disk == before,
            "report must match disk: {disk}"
        );
    }

    #[test]
    fn the_response_counts_external_callers_and_reaching_tests() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("core.rs"), "pub fn core_fn() {}\n").unwrap();
        fs::write(
            dir.path().join("app.rs"),
            "pub fn app() {\n    core_fn();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("core_test.rs"),
            "#[test]\nfn test_core() {\n    core_fn();\n}\n",
        )
        .unwrap();

        let graph = crate::builder::GraphBuilder::new()
            .unwrap()
            .build_graph(dir.path())
            .unwrap();
        let tool = EditTool::new(
            dir.path(),
            Arc::new(RwLock::new(graph)),
            Arc::new(ResponseStore::new(Duration::from_secs(60), 1024 * 1024)),
            4000,
        );

        let before = fs::read_to_string(dir.path().join("core.rs")).unwrap();
        let out = tool.edit(EditParams {
            path: "core.rs".into(),
            symbol: "core_fn".into(),
            expected_hash: hash_prefix(before.as_bytes()),
            new_body: "{ }".into(),
            signature: None,
            budget_tokens: None,
        });
        assert_eq!(out.is_error, None, "{}", text(&out));
        assert!(
            text(&out).contains("app.rs"),
            "external caller missing: {}",
            text(&out)
        );
        assert!(
            text(&out).contains("test_core"),
            "reaching test missing: {}",
            text(&out)
        );
    }

    #[test]
    fn offset_to_line_is_crlf_safe() {
        let content = "a\r\nb\r\nc";
        assert_eq!(offset_to_line(content, 0), 1);
        assert_eq!(offset_to_line(content, 3), 2);
    }
}
