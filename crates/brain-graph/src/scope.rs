//! Name resolution for call edges.
//!
//! Linking a call to a definition by bare name alone is what turned
//! `brain_impact_check new` into 239 fake impacts: every `new()` in the
//! repository became a caller of every one of the 27 definitions of `new`.
//! Resolution has to know where the call is written and what that file brought
//! into scope.
//!
//! The order mirrors how Rust resolves a path:
//!
//! 1. a definition in the same file;
//! 2. a definition reachable through an explicit `use`, or a glob import;
//! 3. a definition exposed through the call's own path (`crate::a::Thing::new`);
//! 4. a definition in the same crate;
//! 5. anywhere, but only when the name is unambiguous.
//!
//! Step 4 is a prior, not a proof — but a same-crate match is far more often
//! right than a cross-crate guess. Step 5 is deliberately last: an ambiguous
//! name with no local evidence resolves to *nothing* rather than to a guess. A
//! missing edge is recoverable; a wrong edge poisons every tool that reads the
//! graph. What step 5 drops is counted, so the tools can say what they do not
//! know.

use petgraph::stable_graph::NodeIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where an imported name is defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportTarget {
    /// A repo-relative file, e.g. `crates/brain-kernel/src/plugin.rs`.
    File(PathBuf),
    /// The standard library or an external crate: never resolvable inside this
    /// repository, so it must not create an edge.
    External,
}

/// What one file brings into scope.
#[derive(Debug, Default, Clone)]
pub struct FileScope {
    /// Local name (last path segment, or the `as` alias) → where it comes from.
    pub imports: HashMap<String, ImportTarget>,
    /// Files brought in by a glob (`use super::*`), whose items are visible
    /// without being named.
    pub globs: Vec<PathBuf>,
}

/// One item of a `use` declaration, after brace expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseItem {
    Named { name: String, path: String },
    Glob { path: String },
}

/// One import statement, with the module it was written in.
///
/// A `use` inside `mod tests` resolves `super` from that module, not from the
/// file's top level, so the enclosing module path travels with the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDecl {
    pub owner: PathBuf,
    /// Enclosing module path, e.g. `crates/brain-graph/src/builder/tests`.
    pub module: String,
    pub item: UseItem,
}

/// Why a call could not be linked to exactly one definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one definition matched.
    Resolved(NodeIndex),
    /// The name is defined, but more than one candidate survives every rule.
    Ambiguous { candidates: usize },
    /// Nothing in the repository defines the name: std, an external crate, or a
    /// macro this index does not model. Not a defect, just out of scope.
    Unknown,
}

/// Turns `use` paths into repository files.
#[derive(Debug, Default)]
pub struct ScopeResolver {
    /// Repo-relative module path without extension → file.
    module_files: HashMap<String, PathBuf>,
    /// Repo-relative file → its module path without extension.
    file_modules: HashMap<PathBuf, String>,
    /// Crate name → its `src` root as a repo-relative module path.
    crate_roots: HashMap<String, String>,
    /// Path prefix identifying a crate, e.g. `crates/brain-kernel/`.
    crate_prefixes: HashMap<String, String>,
    /// Repo-relative file → the crate it belongs to.
    file_crates: HashMap<PathBuf, String>,
    /// Repo-relative file → `crates/x/src/`, the prefix used for same-crate
    /// checks. Cached so resolution never formats a string per call site.
    file_prefixes: HashMap<PathBuf, String>,
}

impl ScopeResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one source file so `use` paths can be mapped onto it.
    ///
    /// `relative` is the path as the graph stores it (repo-relative).
    pub fn register_file(&mut self, relative: &Path, crate_name: &str) {
        let module = module_path_of(relative);
        if module.is_empty() {
            return;
        }

        self.module_files
            .insert(module.clone(), relative.to_path_buf());
        self.file_modules
            .insert(relative.to_path_buf(), module.clone());
        self.file_crates
            .insert(relative.to_path_buf(), crate_name.to_string());
        let prefix = module
            .split_once("/src")
            .map(|(head, _)| format!("{head}/src/"))
            .unwrap_or_else(|| module.clone());
        self.file_prefixes.insert(relative.to_path_buf(), prefix);

        // `lib.rs`, `main.rs` and `mod.rs` name their directory, not a module of
        // their own, so `crate::a::b` must be able to find `src/a/mod.rs`.
        for alias in module_aliases(&module) {
            self.module_files.insert(alias, relative.to_path_buf());
        }

        if let Some(root) = crate_root_of(&module) {
            // Both spellings: the manifest key uses hyphens, the `use` path uses
            // underscores (`brain_kernel::plugin`).
            self.crate_roots.insert(crate_name.to_string(), root.clone());
            self.crate_roots
                .insert(crate_name.replace('-', "_"), root);
            if let Some(prefix) = crate_prefix_of(relative) {
                self.crate_prefixes.insert(crate_name.to_string(), prefix);
            }
        }
    }

    /// Mirrors `register_file`, so a deleted path stops resolving.
    ///
    /// The crate root is only dropped when no surviving file still claims it: a
    /// deleted `plugin.rs` must not take `crate::` down with it.
    pub fn unregister_file(&mut self, relative: &Path) {
        let Some(module) = self.file_modules.remove(relative) else {
            return;
        };
        remove_if_owned(&mut self.module_files, &module, relative);
        self.file_crates.remove(relative);
        self.file_prefixes.remove(relative);
        for alias in module_aliases(&module) {
            remove_if_owned(&mut self.module_files, &alias, relative);
        }

        if let Some(root) = crate_root_of(&module) {
            let still_claimed = self
                .file_modules
                .values()
                .filter_map(|m| crate_root_of(m))
                .any(|other| other == root);
            if !still_claimed {
                let gone: Vec<String> = self
                    .crate_roots
                    .iter()
                    .filter(|(_, kept)| **kept == root)
                    .map(|(name, _)| name.clone())
                    .collect();
                for name in gone {
                    self.crate_roots.remove(&name);
                    self.crate_prefixes.remove(&name);
                }
            }
        }
    }

    pub fn file_module(&self, relative: &Path) -> Option<&str> {
        self.file_modules.get(relative).map(String::as_str)
    }

    /// Cached `crates/x/src/` prefix for a file, used by same-crate resolution.
    pub fn module_prefix(&self, relative: &Path) -> Option<&str> {
        self.file_prefixes.get(relative).map(String::as_str)
    }

    /// Resolves a `use` path written at the top level of `file`.
    pub fn resolve_import(&self, file: &Path, path: &str) -> Option<ImportTarget> {
        let module = self.file_module(file)?;
        self.resolve_import_in(file, module, path)
    }

    /// Resolves a path as written inside `module`, which is either the file's
    /// own module or a module nested below it.
    ///
    /// Accepts `crate::a::b`, `super::super::b`, `self::b`, a bare sibling
    /// module, a workspace member, an external crate, and paths ending in an
    /// item (`crate::a::b::Item`): the longest prefix naming a registered file
    /// wins.
    pub fn resolve_import_in(
        &self,
        file: &Path,
        module: &str,
        path: &str,
    ) -> Option<ImportTarget> {
        let segments = split_path(path);
        if segments.is_empty() {
            return None;
        }

        let head = segments[0];
        let base = if head == "crate" {
            let root = self
                .crate_roots
                .get(&crate_name_for(file))
                .cloned()
                .or_else(|| src_root_of(file))?;
            join(&root, &segments[1..])
        } else if head == "super" {
            // `super` climbs one module per keyword, starting from where the
            // `use` is written — not necessarily the file's top level.
            let mut owner = module.to_string();
            let mut rest: &[&str] = &segments;
            while rest.first().is_some_and(|s| *s == "super") {
                owner = owner
                    .rsplit_once('/')
                    .map(|(head, _)| head.to_string())
                    .unwrap_or_default();
                rest = &rest[1..];
            }
            join(&owner, rest)
        } else if head == "self" {
            join(module, &segments[1..])
        } else if let Some(root) = self.crate_roots.get(head) {
            join(root, &segments[1..])
        } else {
            // A sibling module (`mod util;`, or a crate the workspace did not
            // register) is the only remaining in-repo possibility.
            let parent = module.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
            let sibling = join(parent, &segments);
            if self.is_under_prefix(&sibling) {
                sibling
            } else {
                return Some(ImportTarget::External);
            }
        };

        let mut candidate = base;
        loop {
            if let Some(file) = self.module_files.get(&candidate) {
                return Some(ImportTarget::File(file.clone()));
            }
            candidate.truncate(candidate.rfind('/')?);
        }
    }

    /// True when `module` names a file or a directory that holds one.
    fn is_under_prefix(&self, module: &str) -> bool {
        self.module_files.contains_key(module)
            || self
                .module_files
                .keys()
                .any(|key| key.starts_with(&format!("{module}/")))
    }

    /// The crate a file belongs to, or `None` when nothing registered it.
    fn crate_of(&self, file: &Path) -> Option<&str> {
        self.file_crates.get(file).map(String::as_str)
    }
}

/// Picks the definition a call refers to.
///
/// `call_path` is the full scoped path when the call was written as
/// `crate::a::Thing::new`; it is empty for a bare call.
pub fn pick_target(
    call_name: &str,
    caller_file: &Path,
    scope: &FileScope,
    resolver: &ScopeResolver,
    call_path: Option<&str>,
    definitions: &[NodeIndex],
    graph: &crate::model::ProjectCodeGraph,
) -> Resolution {
    if definitions.is_empty() {
        return Resolution::Unknown;
    }

    let file_of = |index: NodeIndex| {
        graph
            .graph
            .node_weight(index)
            .map(|node| node.file_path.as_path())
            .unwrap_or_else(|| Path::new(""))
    };

    // 1. Same file always wins: a local definition shadows everything.
    let mut same_file = definitions
        .iter()
        .copied()
        .filter(|&index| file_of(index) == caller_file);
    if let (Some(only), None) = (same_file.next(), same_file.next()) {
        return Resolution::Resolved(only);
    }

    // 2. An explicit import decides the answer. If it names a file, that file
    //    is where the call goes. If it names an external crate, an in-repo
    //    definition with the same name is a different symbol — `use tracing::info`
    //    must not link `info!(…)` to a local `fn info`.
    if let Some(target) = scope.imports.get(call_name) {
        return match target {
            ImportTarget::External => Resolution::Unknown,
            ImportTarget::File(file) => {
                match definitions
                    .iter()
                    .copied()
                    .find(|&index| file_of(index) == file)
                {
                    Some(node) => Resolution::Resolved(node),
                    // The import names a module rather than the defining file;
                    // the remaining rules can still place it.
                    None => resolve_within(
                        caller_file,
                        scope,
                        resolver,
                        call_path,
                        definitions,
                        graph,
                    ),
                }
            }
        };
    }

    resolve_within(caller_file, scope, resolver, call_path, definitions, graph)
}

/// Rules 3-6: the call's own path, glob imports, the same crate, and finally an
/// unambiguous global name.
fn resolve_within(
    caller_file: &Path,
    scope: &FileScope,
    resolver: &ScopeResolver,
    call_path: Option<&str>,
    definitions: &[NodeIndex],
    graph: &crate::model::ProjectCodeGraph,
) -> Resolution {
    let file_of = |index: NodeIndex| {
        graph
            .graph
            .node_weight(index)
            .map(|node| node.file_path.as_path())
            .unwrap_or_else(|| Path::new(""))
    };

    // 3. The call's own path (`crate::budget::render(…)`) names its module.
    if let Some(path) = call_path {
        if let Some(ImportTarget::File(file)) = resolver.resolve_import(caller_file, path) {
            if let Some(node) = definitions
                .iter()
                .copied()
                .find(|&index| file_of(index) == file)
            {
                return Resolution::Resolved(node);
            }
        }
    }

    // 4. A glob import makes that module's items visible without naming them.
    let from_globs: Vec<NodeIndex> = definitions
        .iter()
        .copied()
        .filter(|&index| scope.globs.iter().any(|glob| glob == file_of(index)))
        .collect();
    if let [only] = from_globs.as_slice() {
        return Resolution::Resolved(*only);
    }

    // 5. Same crate: the strongest remaining prior. The module prefix is
    // cached at registration, so this does not format a string per candidate.
    if let (Some(prefix), Some(crate_name)) = (
        resolver.module_prefix(caller_file),
        resolver.crate_of(caller_file),
    ) {
        let in_crate: Vec<NodeIndex> = definitions
            .iter()
            .copied()
            .filter(|&index| {
                let file = file_of(index);
                resolver
                    .file_module(file)
                    .is_some_and(|other| other.starts_with(prefix))
                    && resolver.crate_of(file) == Some(crate_name)
            })
            .collect();
        if let [only] = in_crate.as_slice() {
            return Resolution::Resolved(*only);
        }
    }

    // 6. Unique definition anywhere, as a last resort.
    let mut nodes: Vec<NodeIndex> = definitions.to_vec();
    nodes.sort_unstable();
    nodes.dedup();
    match nodes.as_slice() {
        [only] => Resolution::Resolved(*only),
        _ => Resolution::Ambiguous {
            candidates: nodes.len(),
        },
    }
}

/// The type named at the end of a path (`crate::a::Thing` → `Thing`).
pub fn last_segment(path: &str) -> Option<&str> {
    path.rsplit("::").next().map(str::trim).filter(|s| !s.is_empty())
}

/// The single node matching `pred`, or `None` when zero or several match.
fn unique_in(
    definitions: &[(PathBuf, NodeIndex)],
    pred: impl Fn(&Path) -> bool,
) -> Option<NodeIndex> {
    let matches: Vec<NodeIndex> = definitions
        .iter()
        .filter(|(file, _)| pred(file))
        .map(|(_, node)| *node)
        .collect();
    match matches.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// `src/graph/builder.rs` → `src/graph/builder`; `src/lib.rs` → `src/lib`.
fn module_path_of(relative: &Path) -> String {
    let text = relative.to_string_lossy().replace('\\', "/");
    text.strip_suffix(".rs").unwrap_or(&text).to_string()
}

/// Directory-style module files answer to their directory as well.
fn module_aliases(module: &str) -> Vec<String> {
    match module.rsplit_once('/') {
        Some((head, "lib" | "main" | "mod")) => vec![head.to_string()],
        _ => Vec::new(),
    }
}

/// The crate root a `lib.rs`/`main.rs` module stands for.
fn crate_root_of(module: &str) -> Option<String> {
    module
        .rsplit_once('/')
        .filter(|(_, leaf)| *leaf == "lib" || *leaf == "main")
        .map(|(head, _)| head.to_string())
}

/// `crates/brain-kernel/src/plugin.rs` → `crates/brain-kernel/`.
fn crate_prefix_of(file: &Path) -> Option<String> {
    let text = file.to_string_lossy().replace('\\', "/");
    let at = text.find("/src/")?;
    Some(text[..at + 1].to_string())
}

fn remove_if_owned(map: &mut HashMap<String, PathBuf>, key: &str, owner: &Path) {
    if map.get(key).is_some_and(|mapped| mapped == owner) {
        map.remove(key);
    }
}

fn join(base: &str, rest: &[&str]) -> String {
    let mut out = base.to_string();
    for segment in rest {
        if out.is_empty() {
            out.push_str(segment);
        } else {
            out.push('/');
            out.push_str(segment);
        }
    }
    out
}

fn split_path(path: &str) -> Vec<&str> {
    path.split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
        .collect()
}

/// The crate a file belongs to, taken from its own path.
///
/// `crates/brain-kernel/src/plugin.rs` → `brain-kernel`. A file outside any
/// `crates/<name>` layout belongs to the directory that holds its `src`.
pub fn crate_name_for(file: &Path) -> String {
    let text = file.to_string_lossy().replace('\\', "/");
    let segments: Vec<&str> = text.split('/').collect();
    if let Some(at) = segments.iter().position(|s| *s == "crates") {
        if let Some(name) = segments.get(at + 1) {
            return (*name).to_string();
        }
    }
    if let Some(at) = segments.iter().position(|s| *s == "src") {
        if at > 0 {
            return segments[at - 1].to_string();
        }
    }
    "crate".to_string()
}

/// Fallback for a crate with no `lib.rs`/`main.rs` (a fixture directory, or a
/// test tree): everything up to and including the first `src/`.
fn src_root_of(file: &Path) -> Option<String> {
    let text = file.to_string_lossy().replace('\\', "/");
    let mut root = String::new();
    for segment in text.split('/') {
        if !root.is_empty() {
            root.push('/');
        }
        root.push_str(segment);
        if segment == "src" {
            return Some(root);
        }
    }
    None
}

/// Expands one `use` argument into its items, including brace groups.
///
/// `crate::a::{b, c as d}` becomes two named items; `crate::a::*` becomes a
/// glob. Nesting (`a::{b::{c, d}, e}`) is handled by recursing on the joined
/// prefix.
pub fn expand_use(raw: &str) -> Vec<UseItem> {
    let mut out = Vec::new();
    let text = raw
        .trim()
        .trim_start_matches("use ")
        .trim_end_matches(';')
        .trim();
    expand_into(text, &mut out);
    out
}

fn expand_into(text: &str, out: &mut Vec<UseItem>) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    if let Some(open) = text.find('{') {
        let prefix = text[..open].trim().trim_end_matches(':').trim();
        let inner_end = text.rfind('}').unwrap_or(text.len());
        let inner = &text[open + 1..inner_end];
        for part in split_top_level_commas(inner) {
            let combined = if prefix.is_empty() {
                part
            } else {
                format!("{prefix}::{part}")
            };
            expand_into(&combined, out);
        }
        return;
    }

    let (path, alias) = match text.split_once(" as ") {
        Some((path, alias)) => (path.trim(), Some(alias.trim())),
        None => (text, None),
    };

    if let Some(module) = path.strip_suffix('*') {
        let module = module.trim().trim_end_matches(':').trim();
        if !module.is_empty() {
            out.push(UseItem::Glob {
                path: module.to_string(),
            });
        }
        return;
    }

    let name = alias
        .filter(|a| !a.is_empty())
        .or_else(|| path.rsplit("::").next())
        .unwrap_or(path);
    if !name.is_empty() {
        out.push(UseItem::Named {
            name: name.to_string(),
            path: path.to_string(),
        });
    }
}

fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in text.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> ScopeResolver {
        let mut r = ScopeResolver::new();
        for (path, crate_name) in [
            ("crates/brain-kernel/src/lib.rs", "brain-kernel"),
            ("crates/brain-kernel/src/plugin.rs", "brain-kernel"),
            ("crates/brain-kernel/src/budget.rs", "brain-kernel"),
            ("crates/brain-graph/src/lib.rs", "brain-graph"),
            ("crates/brain-graph/src/builder.rs", "brain-graph"),
            ("crates/brain-graph/src/scope.rs", "brain-graph"),
            ("crates/brain-graph/src/graph/mod.rs", "brain-graph"),
            ("crates/brain-graph/src/graph/edge.rs", "brain-graph"),
        ] {
            r.register_file(Path::new(path), crate_name);
        }
        r
    }

    fn idx(n: u32) -> NodeIndex {
        NodeIndex::new(n as usize)
    }

    fn file(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn a_crate_relative_use_resolves_to_the_file() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(Path::new("crates/brain-graph/src/scope.rs"), "crate::builder"),
            Some(ImportTarget::File(file("crates/brain-graph/src/builder.rs")))
        );
    }

    #[test]
    fn an_item_inside_an_imported_module_falls_back_to_that_module() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(
                Path::new("crates/brain-graph/src/scope.rs"),
                "crate::builder::GraphBuilder"
            ),
            Some(ImportTarget::File(file("crates/brain-graph/src/builder.rs")))
        );
    }

    #[test]
    fn super_resolves_to_the_parent_module() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(Path::new("crates/brain-graph/src/scope.rs"), "super::builder"),
            Some(ImportTarget::File(file("crates/brain-graph/src/builder.rs")))
        );
    }

    #[test]
    fn super_inside_an_inline_mod_climbs_from_that_mod() {
        let r = resolver();
        // `mod tests { use super::build_graph; }` inside builder.rs: `super` is
        // the builder module itself, i.e. the file.
        let module = "crates/brain-graph/src/builder/tests";
        assert_eq!(
            r.resolve_import_in(
                Path::new("crates/brain-graph/src/builder.rs"),
                module,
                "super::build_graph"
            ),
            Some(ImportTarget::File(file("crates/brain-graph/src/builder.rs")))
        );
    }

    #[test]
    fn double_super_climbs_two_levels() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(
                Path::new("crates/brain-graph/src/graph/edge.rs"),
                "super::super::scope"
            ),
            Some(ImportTarget::File(file("crates/brain-graph/src/scope.rs")))
        );
    }

    #[test]
    fn a_mod_rs_answers_to_its_directory() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(Path::new("crates/brain-graph/src/builder.rs"), "crate::graph"),
            Some(ImportTarget::File(file("crates/brain-graph/src/graph/mod.rs")))
        );
    }

    #[test]
    fn an_external_crate_is_marked_external_and_never_becomes_an_edge() {
        let r = resolver();
        assert_eq!(
            r.resolve_import(Path::new("crates/brain-graph/src/scope.rs"), "std::collections"),
            Some(ImportTarget::External)
        );
    }

    #[test]
    fn another_workspace_member_resolves_through_its_crate_root() {
        let r = resolver();
        // Underscored spelling, as a `use` writes it. The module segment wins
        // over the crate root, so the edge lands on plugin.rs rather than on
        // lib.rs — a narrower target is always the better one.
        assert_eq!(
            r.resolve_import(
                Path::new("crates/brain-graph/src/scope.rs"),
                "brain_kernel::plugin::Registry"
            ),
            Some(ImportTarget::File(file("crates/brain-kernel/src/plugin.rs")))
        );
        assert_eq!(
            r.resolve_import(
                Path::new("crates/brain-graph/src/scope.rs"),
                "brain_kernel::Registry"
            ),
            Some(ImportTarget::File(file("crates/brain-kernel/src/lib.rs")))
        );
    }

    #[test]
    fn a_crate_root_without_lib_rs_still_resolves_crate_paths() {
        let mut r = ScopeResolver::new();
        r.register_file(Path::new("fixture/src/auth.rs"), "fixture");
        r.register_file(Path::new("fixture/src/crypto.rs"), "fixture");
        assert_eq!(
            r.resolve_import(Path::new("fixture/src/auth.rs"), "crate::crypto"),
            Some(ImportTarget::File(file("fixture/src/crypto.rs")))
        );
    }

    #[test]
    fn unregister_stops_a_deleted_file_from_resolving() {
        let mut r = resolver();
        r.unregister_file(Path::new("crates/brain-graph/src/scope.rs"));
        assert!(
            r.resolve_import(Path::new("crates/brain-graph/src/builder.rs"), "crate::scope")
                .is_none()
                || r.resolve_import(
                    Path::new("crates/brain-graph/src/builder.rs"),
                    "crate::scope"
                ) == Some(ImportTarget::File(file("crates/brain-graph/src/lib.rs"))),
            "a deleted module must not resolve to its own file"
        );
    }

    #[test]
    fn deleting_a_file_keeps_the_crate_root_alive() {
        let mut r = resolver();
        r.unregister_file(Path::new("crates/brain-kernel/src/plugin.rs"));
        assert_eq!(
            r.resolve_import(Path::new("crates/brain-graph/src/scope.rs"), "crate::builder"),
            Some(ImportTarget::File(file("crates/brain-graph/src/builder.rs")))
        );
    }

    #[test]
    fn a_definition_in_the_same_file_beats_everything() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/builder.rs");
        let definitions = vec![
            (file("crates/brain-graph/src/other.rs"), idx(7)),
            (caller.clone(), idx(3)),
        ];

        assert_eq!(
            pick_target("build_graph", &caller, &FileScope::default(), &r, None, &definitions),
            Resolution::Resolved(idx(3))
        );
    }

    #[test]
    fn an_imported_name_resolves_only_to_its_own_file() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let mut scope = FileScope::default();
        scope.imports.insert(
            "render".to_string(),
            ImportTarget::File(file("crates/brain-graph/src/budget.rs")),
        );

        let definitions = vec![
            (file("crates/brain-kernel/src/budget.rs"), idx(1)),
            (file("crates/brain-graph/src/budget.rs"), idx(2)),
        ];
        assert_eq!(
            pick_target("render", &caller, &scope, &r, None, &definitions),
            Resolution::Resolved(idx(2))
        );
    }

    #[test]
    fn an_ambiguous_name_with_no_import_creates_no_edge() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        // Two definitions in *other* crates: no local evidence at all.
        let definitions = vec![
            (file("crates/brain-kernel/src/plugin.rs"), idx(1)),
            (file("crates/brain-exec/src/cache.rs"), idx(2)),
        ];

        assert_eq!(
            pick_target("zip", &caller, &FileScope::default(), &r, None, &definitions),
            Resolution::Ambiguous { candidates: 2 },
            "guessing between two definitions is what poisoned the graph"
        );
    }

    #[test]
    fn a_same_crate_definition_wins_over_a_foreign_one() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let definitions = vec![
            (file("crates/brain-kernel/src/plugin.rs"), idx(1)),
            (file("crates/brain-graph/src/builder.rs"), idx(2)),
        ];

        assert_eq!(
            pick_target("zip", &caller, &FileScope::default(), &r, None, &definitions),
            Resolution::Resolved(idx(2))
        );
    }

    #[test]
    fn an_external_import_blocks_a_cross_crate_match() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let mut scope = FileScope::default();
        scope
            .imports
            .insert("info".to_string(), ImportTarget::External);
        let definitions = vec![(file("crates/brain-graph/src/builder.rs"), idx(1))];

        assert_eq!(
            pick_target("info", &caller, &scope, &r, None, &definitions),
            Resolution::Unknown,
            "`use tracing::info` means this `info!` is not the local `fn info`"
        );
    }

    #[test]
    fn a_name_defined_nowhere_is_unknown_not_ambiguous() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        assert_eq!(
            pick_target("println", &caller, &FileScope::default(), &r, None, &[]),
            Resolution::Unknown
        );
    }

    #[test]
    fn a_unique_name_still_resolves_across_crates() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let definitions = vec![(file("crates/brain-kernel/src/plugin.rs"), idx(9))];

        assert_eq!(
            pick_target("Registry", &caller, &FileScope::default(), &r, None, &definitions),
            Resolution::Resolved(idx(9))
        );
    }

    #[test]
    fn same_crate_breaks_a_tie_between_two_definitions() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let definitions = vec![
            (file("crates/brain-graph/src/builder.rs"), idx(4)),
            (file("crates/brain-kernel/src/plugin.rs"), idx(5)),
        ];

        assert_eq!(
            pick_target("build_graph", &caller, &FileScope::default(), &r, None, &definitions),
            Resolution::Resolved(idx(4))
        );
    }

    #[test]
    fn the_call_path_pins_the_module() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let definitions = vec![
            (file("crates/brain-kernel/src/budget.rs"), idx(1)),
            (file("crates/brain-graph/src/builder.rs"), idx(2)),
        ];

        assert_eq!(
            pick_target(
                "GraphBuilder",
                &caller,
                &FileScope::default(),
                &r,
                Some("crate::builder::GraphBuilder"),
                &definitions
            ),
            Resolution::Resolved(idx(2))
        );
    }

    #[test]
    fn a_glob_import_makes_its_module_the_answer() {
        let r = resolver();
        let caller = file("crates/brain-graph/src/scope.rs");
        let scope = FileScope {
            imports: HashMap::new(),
            globs: vec![file("crates/brain-graph/src/builder.rs")],
        };
        let definitions = vec![
            (file("crates/brain-graph/src/builder.rs"), idx(4)),
            (file("crates/brain-kernel/src/plugin.rs"), idx(5)),
        ];

        assert_eq!(
            pick_target("build_graph", &caller, &scope, &r, None, &definitions),
            Resolution::Resolved(idx(4))
        );
    }

    #[test]
    fn crate_name_is_read_from_the_path() {
        assert_eq!(
            crate_name_for(Path::new("crates/brain-kernel/src/plugin.rs")),
            "brain-kernel"
        );
    }

    #[test]
    fn expand_use_handles_aliases_globs_and_brace_groups() {
        assert_eq!(
            expand_use("crate::a::Foo"),
            vec![UseItem::Named {
                name: "Foo".to_string(),
                path: "crate::a::Foo".to_string()
            }]
        );
        assert_eq!(
            expand_use("crate::a::Foo as Bar"),
            vec![UseItem::Named {
                name: "Bar".to_string(),
                path: "crate::a::Foo".to_string()
            }]
        );
        assert_eq!(
            expand_use("crate::a::*"),
            vec![UseItem::Glob {
                path: "crate::a".to_string()
            }]
        );
        assert_eq!(
            expand_use("crate::a::{Foo, bar as baz, *}"),
            vec![
                UseItem::Named {
                    name: "Foo".to_string(),
                    path: "crate::a::Foo".to_string()
                },
                UseItem::Named {
                    name: "baz".to_string(),
                    path: "crate::a::bar".to_string()
                },
                UseItem::Glob {
                    path: "crate::a".to_string()
                },
            ]
        );
    }

    #[test]
    fn nested_brace_groups_expand_fully() {
        assert_eq!(
            expand_use("crate::{a::{b, c}, d}"),
            vec![
                UseItem::Named {
                    name: "b".to_string(),
                    path: "crate::a::b".to_string()
                },
                UseItem::Named {
                    name: "c".to_string(),
                    path: "crate::a::c".to_string()
                },
                UseItem::Named {
                    name: "d".to_string(),
                    path: "crate::d".to_string()
                },
            ]
        );
    }
}
