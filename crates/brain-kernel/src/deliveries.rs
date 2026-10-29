//! Session-scoped memory of what the agent already received.
//!
//! The daemon is one process per client session, so "already seen" has an
//! unambiguous scope: what this process delivered. The ledger records the
//! *content* of each delivered symbol together with the hash of the file at
//! delivery time, then answers three questions on the next request:
//!
//! - the file did not change → the body is identical to what the agent
//!   received; it may be pointed at instead of re-sent;
//! - the file changed → the changed lines are what the agent has not seen;
//! - the previous delivery was a partial view (a truncated chunk) → a diff
//!   would lie about the rest, so the body comes back whole.
//!
//! Two rules keep it honest. A pointer always carries a **live** handle to the
//! body (the ledger revives it from the stored text when the old one expired),
//! and a symbol is only omitted a limited number of times before the body is
//! served again — a silent block is worse than a token.
//!
//! The key is the symbol's identity (`path` + qualified name), not its byte
//! span: a span moves whenever a line above it is edited, and a moved key would
//! turn every change into "never seen".

use crate::budget::estimate_tokens;
use blake3::Hash;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT_ENTRIES: usize = 512;
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// After this many omissions of the same symbol the body is served again.
const MAX_OMISSIONS: usize = 2;

/// Above this many lines a diff stops being cheaper than the body.
const DIFF_MAX_LINES: usize = 600;

/// Lines of unchanged context printed around each changed hunk.
const DIFF_CONTEXT: usize = 1;

/// What the ledger knows about a request for a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Never delivered in this session.
    Fresh,
    /// Delivered, and the file has not changed (or changed elsewhere).
    Seen(Seen),
    /// Delivered, and the symbol itself changed: only the diff was not seen.
    Changed(Changed),
    /// Delivered, but pointing at the old text would mislead. Serve the body.
    Full {
        reason: FullReason,
        previous_tool: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The text that was delivered, kept so the pointer can be revived.
    pub text: String,
    pub handle: Option<String>,
    pub tool: String,
    /// Short hash of the file at delivery time — what makes the claim checkable.
    pub hash: String,
    pub lines: usize,
    pub tokens: usize,
    /// The file changed since the delivery, but not inside this symbol.
    pub file_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub diff: String,
    /// Short hashes, for the note that says *what* changed.
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullReason {
    /// The same symbol was already omitted too many times.
    TooManyRepeats,
    /// The delivered text was a chunk of the symbol: a pointer would hide the rest.
    PartialDelivery,
    /// The previous delivery was a chunk and the file changed since.
    PartialBefore,
    /// The symbol is too large to diff line by line.
    TooLargeToDiff,
}

/// A symbol that was actually put in front of the agent.
#[derive(Debug, Clone)]
pub struct Delivery<'a> {
    pub tool: &'a str,
    pub path: &'a Path,
    /// Stable identity inside the file: `container::name` or `name`.
    pub id: &'a str,
    pub file_hash: Hash,
    pub text: &'a str,
    /// `false` when the delivered text was a chunk of the symbol, not all of it.
    pub complete: bool,
    pub handle: Option<String>,
}

struct Entry {
    tool: String,
    path: PathBuf,
    id: String,
    file_hash: Hash,
    text: String,
    complete: bool,
    handle: Option<String>,
    delivered_at: u64,
    omissions: usize,
    bytes: usize,
}

struct State {
    entries: Vec<Entry>,
    bytes: usize,
    clock: u64,
}

/// Bounded, in-memory, per-process. Losing an entry only costs a repeated
/// body — never a wrong answer.
pub struct DeliveryLedger {
    max_entries: usize,
    max_bytes: usize,
    state: Mutex<State>,
}

impl DeliveryLedger {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            state: Mutex::new(State {
                entries: Vec::new(),
                bytes: 0,
                clock: 0,
            }),
        }
    }

    /// Decides what to do with a request for `id`.
    ///
    /// `current_text` is the symbol as it is on disk *now*: it is only used to
    /// build the diff when the file hash says something changed.
    pub fn check(
        &self,
        path: &Path,
        id: &str,
        file_hash: &Hash,
        current_text: &str,
        start_line: usize,
    ) -> Verdict {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clock += 1;
        let clock = state.clock;

        let Some(entry) = state
            .entries
            .iter_mut()
            .find(|e| e.path == path && e.id == id)
        else {
            return Verdict::Fresh;
        };
        entry.delivered_at = clock;

        if entry.omissions >= MAX_OMISSIONS {
            entry.omissions = 0;
            return Verdict::Full {
                reason: FullReason::TooManyRepeats,
                previous_tool: entry.tool.clone(),
            };
        }

        if entry.file_hash == *file_hash {
            if !entry.complete {
                return Verdict::Full {
                    reason: FullReason::PartialDelivery,
                    previous_tool: entry.tool.clone(),
                };
            }
            return Verdict::Seen(seen_of(entry, false));
        }

        if !entry.complete {
            return Verdict::Full {
                reason: FullReason::PartialBefore,
                previous_tool: entry.tool.clone(),
            };
        }

        if entry.text.trim_end() == current_text.trim_end() {
            // The file moved on outside this symbol: the body is still the one
            // the agent already has.
            return Verdict::Seen(seen_of(entry, true));
        }

        match diff_lines(&entry.text, current_text, start_line, DIFF_CONTEXT) {
            Some(diff) if !diff.is_empty() => Verdict::Changed(Changed {
                diff,
                from: short_hash(&entry.file_hash),
                to: short_hash(file_hash),
            }),
            Some(_) => Verdict::Seen(seen_of(entry, true)),
            None => Verdict::Full {
                reason: FullReason::TooLargeToDiff,
                previous_tool: entry.tool.clone(),
            },
        }
    }

    /// Remembers where the delivered text can be recovered from. A pointer is
    /// only as good as its handle.
    pub fn remember_handle(&self, path: &Path, id: &str, handle: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|e| e.path == path && e.id == id)
        {
            entry.handle = Some(handle.to_string());
        }
    }

    /// Counts that a pointer was actually put in the response. Only then does
    /// the omission budget move.
    pub fn note_omitted(&self, path: &Path, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|e| e.path == path && e.id == id)
        {
            entry.omissions += 1;
        }
    }

    /// Records what the agent just received. Replaces any previous entry for
    /// the same symbol, because the newest delivery is the one that counts.
    pub fn record(&self, delivery: Delivery<'_>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clock += 1;
        let clock = state.clock;

        let bytes = delivery.text.len() + delivery.path.to_string_lossy().len() + 64;
        if let Some(previous) = state
            .entries
            .iter()
            .position(|e| e.path == delivery.path && e.id == delivery.id)
        {
            state.bytes = state.bytes.saturating_sub(state.entries[previous].bytes);
            state.entries.remove(previous);
        }

        state.entries.push(Entry {
            tool: delivery.tool.to_string(),
            path: delivery.path.to_path_buf(),
            id: delivery.id.to_string(),
            file_hash: delivery.file_hash,
            text: delivery.text.to_string(),
            complete: delivery.complete,
            handle: delivery.handle,
            delivered_at: clock,
            omissions: 0,
            bytes,
        });
        state.bytes += bytes;

        while state.entries.len() > self.max_entries || state.bytes > self.max_bytes {
            // Never evict the entry that was just inserted: with a tiny ceiling
            // that would make the ledger forget everything, including the call
            // it is answering.
            if state.entries.len() == 1 {
                break;
            }
            let Some(oldest) = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.delivered_at)
                .map(|(i, _)| i)
            else {
                break;
            };
            let entry = state.entries.remove(oldest);
            state.bytes = state.bytes.saturating_sub(entry.bytes);
        }
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn seen_of(entry: &Entry, file_changed: bool) -> Seen {
    Seen {
        text: entry.text.clone(),
        handle: entry.handle.clone(),
        tool: entry.tool.clone(),
        hash: short_hash(&entry.file_hash),
        lines: entry.text.lines().count(),
        tokens: estimate_tokens(&entry.text),
        file_changed,
    }
}

/// The body of a pointer: says exactly what was omitted, how much it costs and
/// where to recover it. A pointer without a handle would be a dead end.
pub fn pointer_body(seen: &Seen, handle: &str) -> String {
    let mut out = format!(
        "> **Já entregue nesta sessão** ({} · blake3 `{}` · {} linhas) — corpo em `brain_peek` `{handle}`.\n",
        seen.tool, seen.hash, seen.lines
    );
    if seen.file_changed {
        out.push_str("> O arquivo mudou fora deste símbolo; o corpo é o mesmo.\n");
    }
    out
}

/// The diff note plus its hunks. Never truncated: a cut diff reads as "nothing
/// else changed".
pub fn changed_body(changed: &Changed) -> String {
    format!(
        "> **Mudou desde a entrega** (blake3 `{}` → `{}`): só as linhas alteradas abaixo; o resto do corpo é o que você já tem.\n\n```diff\n{}```\n",
        changed.from, changed.to, changed.diff
    )
}

pub fn full_note(reason: FullReason) -> &'static str {
    match reason {
        FullReason::TooManyRepeats => {
            "> Terceira repetição deste símbolo: entregando o corpo em vez de omitir de novo."
        }
        FullReason::PartialDelivery => {
            "> O trecho entregue antes era um recorte do símbolo; segue o corpo inteiro para você não trabalhar com metade."
        }
        FullReason::PartialBefore => {
            "> O trecho entregue antes era um recorte; o arquivo mudou, então segue o corpo atual inteiro."
        }
        FullReason::TooLargeToDiff => {
            "> Símbolo grande demais para diff; segue o corpo atual inteiro."
        }
    }
}

pub fn short_hash(hash: &Hash) -> String {
    hash.to_hex()[..12].to_string()
}

/// The hash a delivery records: the whole file, so a change anywhere invalidates
/// the pointer and forces a diff instead of a stale answer.
pub fn content_hash(content: &str) -> Hash {
    blake3::hash(content.as_bytes())
}

/// A pointer is only as good as its handle. Returns a live handle for the
/// stored text, reviving it from the ledger when the previous one expired.
pub fn live_handle(
    store: &crate::store::ResponseStore,
    ledger: &DeliveryLedger,
    seen: &Seen,
    path: &Path,
    id: &str,
) -> String {
    if let Some(handle) = &seen.handle {
        if store.get(handle).is_some() {
            return handle.clone();
        }
    }
    let handle = store.insert(seen.text.clone());
    ledger.remember_handle(path, id, &handle);
    handle
}

/// Byte range of the 1-based `start_line..=end_line` span, matching the line
/// numbers a snippet reports.
pub fn line_span(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> Option<std::ops::Range<usize>> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let mut start = None;
    let mut end = None;
    let mut offset = 0usize;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let number = index + 1;
        if number == start_line {
            start = Some(offset);
        }
        offset += line.len();
        if number >= end_line {
            end = Some(offset);
            break;
        }
    }
    // An end past the last line clamps to the end of the file.
    let end = end.or_else(|| start.map(|_| content.len()));
    Some(start?..end?)
}

/// Line diff with `context` unchanged lines around each hunk, or `None` when
/// the inputs are too large to diff cheaply.
pub fn diff_lines(old: &str, new: &str, old_start_line: usize, context: usize) -> Option<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len() > DIFF_MAX_LINES || new_lines.len() > DIFF_MAX_LINES {
        return None;
    }

    #[derive(Clone, Copy)]
    enum Op {
        /// The new-side line index.
        Equal(usize),
        Delete(usize),
        Insert(usize),
    }

    let (n, m) = (old_lines.len(), new_lines.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old_lines[i] == new_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push(Op::Equal(j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert(j));
        j += 1;
    }

    if !ops.iter().any(|op| !matches!(op, Op::Equal(..))) {
        return Some(String::new());
    }

    let old_index_at = |upto: usize| -> usize {
        ops[..upto]
            .iter()
            .filter(|op| matches!(op, Op::Equal(..) | Op::Delete(..)))
            .count()
    };

    let mut out = String::new();
    let mut idx = 0usize;
    let mut emitted_until = 0usize;
    while idx < ops.len() {
        if matches!(ops[idx], Op::Equal(..)) {
            idx += 1;
            continue;
        }

        let run_start = idx;
        let mut run_end = idx;
        while run_end < ops.len() && !matches!(ops[run_end], Op::Equal(..)) {
            run_end += 1;
        }

        let from = run_start.saturating_sub(context).max(emitted_until);
        let to = (run_end + context).min(ops.len());
        out.push_str(&format!("@@ {} @@\n", old_start_line + old_index_at(from)));
        for op in &ops[from..to] {
            match *op {
                Op::Equal(new_line) => out.push_str(&format!(" {}\n", new_lines[new_line])),
                Op::Delete(old_line) => out.push_str(&format!("-{}\n", old_lines[old_line])),
                Op::Insert(new_line) => out.push_str(&format!("+{}\n", new_lines[new_line])),
            }
        }
        emitted_until = to;
        idx = to;
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(text: &str) -> Hash {
        blake3::hash(text.as_bytes())
    }

    fn ledger() -> DeliveryLedger {
        DeliveryLedger::new(DEFAULT_ENTRIES, DEFAULT_MAX_BYTES)
    }

    fn record(ledger: &DeliveryLedger, id: &str, text: &str) {
        ledger.record(Delivery {
            tool: "brain_read",
            path: Path::new("a.rs"),
            id,
            file_hash: hash(text),
            text,
            complete: true,
            handle: Some("h_first".to_string()),
        });
    }

    #[test]
    fn an_unseen_symbol_is_fresh() {
        let ledger = ledger();
        let verdict = ledger.check(Path::new("a.rs"), "f", &hash("x"), "x", 1);
        assert_eq!(verdict, Verdict::Fresh);
    }

    #[test]
    fn the_same_symbol_with_the_same_hash_is_seen() {
        let ledger = ledger();
        let body = "fn a() {}\n";
        record(&ledger, "a", body);

        match ledger.check(Path::new("a.rs"), "a", &hash(body), body, 1) {
            Verdict::Seen(seen) => {
                assert_eq!(seen.text, body);
                assert_eq!(seen.handle.as_deref(), Some("h_first"));
                assert_eq!(seen.tool, "brain_read");
                assert!(!seen.file_changed);
            }
            other => panic!("expected Seen, got {other:?}"),
        }
    }

    #[test]
    fn a_changed_file_yields_only_the_changed_lines() {
        let ledger = ledger();
        let old = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        let new = "fn a() {\n    let x = 1;\n    let y = 3;\n}\n";
        record(&ledger, "a", old);

        match ledger.check(Path::new("a.rs"), "a", &hash(new), new, 10) {
            Verdict::Changed(changed) => {
                assert!(changed.diff.contains("-    let y = 2;"), "{}", changed.diff);
                assert!(changed.diff.contains("+    let y = 3;"), "{}", changed.diff);
                assert!(!changed.diff.contains("-fn a"), "unchanged lines stay out");
                assert!(changed.diff.contains("@@ 11 @@"), "{}", changed.diff);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn an_edited_symbol_keeps_its_identity_when_lines_move() {
        let ledger = ledger();
        let body = "fn a() {\n    1\n}\n";
        record(&ledger, "a", body);
        // Same symbol, different length, different position in the file.
        let new = "fn a() {\n    1 + 1\n}\n";
        match ledger.check(Path::new("a.rs"), "a", &hash(new), new, 40) {
            Verdict::Changed(changed) => assert!(changed.diff.contains("+    1 + 1")),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn a_change_outside_the_symbol_is_still_a_pointer() {
        let ledger = ledger();
        let body = "fn a() {}\n";
        record(&ledger, "a", body);

        match ledger.check(Path::new("a.rs"), "a", &hash("other"), body, 1) {
            Verdict::Seen(seen) => assert!(seen.file_changed),
            other => panic!("expected Seen, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_delivery_is_served_whole_even_when_unchanged() {
        let ledger = ledger();
        ledger.record(Delivery {
            tool: "brain_search",
            path: Path::new("a.rs"),
            id: "long_fn",
            file_hash: hash("v1"),
            text: "recorte de 10 linhas",
            complete: false,
            handle: None,
        });

        match ledger.check(Path::new("a.rs"), "long_fn", &hash("v1"), "v1 inteiro", 1) {
            Verdict::Full { reason, .. } => assert_eq!(reason, FullReason::PartialDelivery),
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_delivery_is_never_diffed() {
        let ledger = ledger();
        ledger.record(Delivery {
            tool: "brain_search",
            path: Path::new("a.rs"),
            id: "long_fn",
            file_hash: hash("v1"),
            text: "primeiras 10 linhas",
            complete: false,
            handle: None,
        });

        match ledger.check(Path::new("a.rs"), "long_fn", &hash("v2"), "v2 inteiro", 1) {
            Verdict::Full { reason, .. } => assert_eq!(reason, FullReason::PartialBefore),
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn a_symbol_is_not_omitted_forever() {
        let ledger = ledger();
        let body = "fn a() {}\n";
        record(&ledger, "a", body);

        for _ in 0..MAX_OMISSIONS {
            assert!(matches!(
                ledger.check(Path::new("a.rs"), "a", &hash(body), body, 1),
                Verdict::Seen(_)
            ));
            ledger.note_omitted(Path::new("a.rs"), "a");
        }

        match ledger.check(Path::new("a.rs"), "a", &hash(body), body, 1) {
            Verdict::Full { reason, .. } => assert_eq!(reason, FullReason::TooManyRepeats),
            other => panic!("expected Full, got {other:?}"),
        }
        // The counter restarts, so the next request is a pointer again.
        assert!(matches!(
            ledger.check(Path::new("a.rs"), "a", &hash(body), body, 1),
            Verdict::Seen(_)
        ));
    }

    #[test]
    fn recording_the_same_symbol_replaces_the_previous_entry() {
        let ledger = ledger();
        record(&ledger, "a", "v1");
        record(&ledger, "a", "v2");
        assert_eq!(ledger.len(), 1);

        match ledger.check(Path::new("a.rs"), "a", &hash("v2"), "v2", 1) {
            Verdict::Seen(seen) => assert_eq!(seen.text, "v2"),
            other => panic!("expected Seen, got {other:?}"),
        }
    }

    #[test]
    fn the_oldest_entry_is_evicted_at_the_ceiling() {
        let ledger = DeliveryLedger::new(2, DEFAULT_MAX_BYTES);
        record(&ledger, "a", "aaa");
        record(&ledger, "b", "bbb");
        record(&ledger, "c", "ccc");

        assert_eq!(ledger.len(), 2);
        assert!(matches!(
            ledger.check(Path::new("a.rs"), "a", &hash("aaa"), "aaa", 1),
            Verdict::Fresh
        ));
    }

    #[test]
    fn a_single_entry_survives_a_tiny_byte_ceiling() {
        let ledger = DeliveryLedger::new(DEFAULT_ENTRIES, 1);
        record(&ledger, "a", "some body");
        assert_eq!(ledger.len(), 1, "the answer's own entry is not evicted");
    }

    #[test]
    fn a_remembered_handle_is_returned() {
        let ledger = ledger();
        record(&ledger, "a", "body");
        ledger.remember_handle(Path::new("a.rs"), "a", "h_new");

        match ledger.check(Path::new("a.rs"), "a", &hash("body"), "body", 1) {
            Verdict::Seen(seen) => assert_eq!(seen.handle.as_deref(), Some("h_new")),
            other => panic!("expected Seen, got {other:?}"),
        }
    }

    #[test]
    fn a_line_span_covers_whole_lines() {
        let content = "a\nbb\nccc\n";
        assert_eq!(&content[line_span(content, 2, 3).unwrap()], "bb\nccc\n");
        assert_eq!(&content[line_span(content, 1, 1).unwrap()], "a\n");
        // Past the last line it clamps instead of panicking.
        assert_eq!(&content[line_span(content, 3, 9).unwrap()], "ccc\n");
        assert!(line_span(content, 9, 10).is_none());
    }

    #[test]
    fn diff_shows_context_and_never_splits_a_hunk() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nC\nd\ne\n";
        let diff = diff_lines(old, new, 1, 1).unwrap();
        assert!(diff.contains("@@ 2 @@"), "{diff}");
        assert!(diff.contains(" b\n-c\n+C\n d\n"), "{diff}");
    }

    #[test]
    fn identical_texts_produce_an_empty_diff() {
        assert_eq!(diff_lines("a\nb\n", "a\nb\n", 1, 1).unwrap(), "");
    }

    #[test]
    fn huge_spans_are_not_diffed() {
        let old = "a\n".repeat(DIFF_MAX_LINES + 1);
        let new = "b\n".repeat(DIFF_MAX_LINES + 1);
        assert!(diff_lines(&old, &new, 1, 1).is_none());
    }
}
