//! Local session state: an append-only event log per session under
//! `.brain/state/sessions/`. This is deliberately not a knowledge base —
//! durable facts belong in `brain_ledger`. Here lives the working trail that
//! lets the next session resume instead of restart.

use crate::budget::Section;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_DIR: &str = ".brain/state";
const MAX_SESSIONS: usize = 32;
const MAX_EVENTS: usize = 4000;
const MAX_MEMORY_NOTES: usize = 200;
const MAX_MEMORY_TASKS: usize = 100;
const MAX_EVENT_FILE_BYTES: u64 = 8 * 1024 * 1024;
const RESUME_MAX_CHARS: usize = 1600;
const TEXT_MAX_CHARS: usize = 240;
const INFERENCE_TTL_SECS: u64 = 7200;

/// One line in `events.jsonl`. Flat and optional-heavy on purpose: old lines
/// keep parsing when a field is added, and a corrupt line loses only itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEvent {
    pub v: u8,
    pub ts: u64,
    pub session: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

impl StateEvent {
    fn new(session: &str, kind: &str) -> Self {
        Self {
            v: 1,
            ts: now_secs(),
            session: session.to_string(),
            kind: kind.to_string(),
            tool: None,
            target: None,
            path: None,
            symbol: None,
            text: None,
            tags: None,
            task_id: None,
            status: None,
            ok: None,
            ms: None,
            bytes: None,
            hash: None,
        }
    }
}

/// What the registry knows about a completed tool call. No arguments and no
/// bodies: the state trail must not become a second copy of the code.
pub struct ToolRecord<'a> {
    pub tool: &'a str,
    pub target: Option<&'a str>,
    pub path: Option<&'a str>,
    pub symbol: Option<&'a str>,
    pub ok: bool,
    pub ms: u64,
    pub bytes: usize,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    v: u8,
    session: String,
    root: String,
    started_at: u64,
    head: Option<String>,
}

/// A session owns one directory and one writer. Opening a store never writes:
/// directories are created on the first event, so a read-only client leaves no
/// trace behind.
#[derive(Default)]
struct Inference {
    failure: Option<Failure>,
}

#[derive(Default)]
struct Access {
    count: usize,
    last_ts: u64,
}

struct Failure {
    kind: String,
    ts: u64,
    changed: BTreeSet<String>,
    changed_any: bool,
}

pub struct StateStore {
    root: PathBuf,
    session: String,
    dir: PathBuf,
    previous: Option<PathBuf>,
    append_lock: Mutex<()>,
    events: AtomicUsize,
    inference: Mutex<Inference>,
    access: Mutex<BTreeMap<String, Access>>,
    seeded: AtomicBool,
}

impl StateStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let session = format!("{:020}-{}", now_nanos(), std::process::id());
        let dir = root.join(STATE_DIR).join("sessions").join(&session);
        let previous = previous_session(&root, &session);
        Self {
            root,
            session,
            dir,
            previous,
            append_lock: Mutex::new(()),
            events: AtomicUsize::new(0),
            inference: Mutex::new(Inference::default()),
            access: Mutex::new(BTreeMap::new()),
            seeded: AtomicBool::new(false),
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn record_tool(&self, record: ToolRecord<'_>) {
        let mut event = StateEvent::new(&self.session, "tool");
        event.tool = Some(clean(record.tool));
        event.target = record.target.map(clean);
        event.path = record.path.map(clean);
        event.symbol = record.symbol.map(clean);
        event.ok = Some(record.ok);
        event.ms = Some(record.ms);
        event.bytes = Some(record.bytes);
        if let Err(e) = self.record(event.clone()) {
            tracing::warn!(error = %e, tool = record.tool, "could not append session state");
            return;
        }
        self.observe_tool(&event);
        self.record_access(&event);
    }

    fn observe_tool(&self, event: &StateEvent) {
        let tool = event.tool.as_deref().unwrap_or_default();

        if tool == "brain_exec" {
            let kind = event.target.as_deref().unwrap_or_default();
            let ok = event.ok.unwrap_or(true);
            let mut inference = self
                .inference
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            if !ok {
                inference.failure = Some(Failure {
                    kind: kind.to_string(),
                    ts: event.ts,
                    changed: BTreeSet::new(),
                    changed_any: false,
                });
                return;
            }

            let Some(failure) = inference.failure.as_ref() else {
                return;
            };
            if failure.kind != kind {
                return;
            }
            let failure = inference.failure.take().expect("checked above");
            drop(inference);
            if failure.changed_any
                && event.ts.saturating_sub(failure.ts) <= INFERENCE_TTL_SECS
            {
                self.record_inferred(&failure, event.ts);
            }
            return;
        }

        if !is_mutating(tool) {
            return;
        }
        let mut inference = self
            .inference
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = &mut inference.failure {
            failure.changed_any = true;
            if let Some(path) = &event.path {
                failure.changed.insert(path.clone());
            }
        }
    }

    fn record_inferred(&self, failure: &Failure, ts: u64) {
        let where_changed = if failure.changed.is_empty() {
            "alterações no código".to_string()
        } else {
            let paths = failure
                .changed
                .iter()
                .take(4)
                .map(|path| format!("`{path}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("alterações em {paths}")
        };
        let text = format!(
            "Inferido: `brain_exec {}` falhou e passou após {where_changed}.",
            failure.kind
        );
        let mut event = StateEvent::new(&self.session, "note");
        event.ts = ts;
        event.text = Some(clean(&text));
        event.tags = Some(vec!["inferred".to_string(), "exec".to_string()]);
        let _ = self.record(event);
    }

    fn record_access(&self, event: &StateEvent) {
        let Some(path) = &event.path else {
            return;
        };
        let mut access = self.access.lock().unwrap_or_else(|e| e.into_inner());
        let entry = access.entry(path.clone()).or_default();
        entry.count += 1;
        entry.last_ts = event.ts;
    }

    pub fn hot_paths(&self, limit: usize) -> BTreeMap<String, f32> {
        self.seed_from_previous();
        let access = self.access.lock().unwrap_or_else(|e| e.into_inner());
        let max = access
            .values()
            .map(|entry| entry.count)
            .max()
            .unwrap_or(0)
            .max(1) as f32;
        let now = now_secs();
        let weight = |entry: &Access| {
            let count = entry.count as f32 / max;
            let age = now.saturating_sub(entry.last_ts) as f32;
            count * 0.8 + (1.0 / (1.0 + age / 1800.0)) * 0.2
        };
        let mut rows: Vec<(String, f32)> = access
            .iter()
            .map(|(path, entry)| (relative_path(&self.root, path), weight(entry)))
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(limit.max(1));
        rows.into_iter().collect()
    }

    fn seed_from_previous(&self) {
        if self.seeded.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(previous) = &self.previous else {
            return;
        };
        let Ok(events) = read_events(previous) else {
            return;
        };
        let mut access = self.access.lock().unwrap_or_else(|e| e.into_inner());
        for event in &events {
            if let Some(path) = &event.path {
                access.entry(path.clone()).or_insert(Access {
                    count: 1,
                    last_ts: event.ts,
                });
            }
        }
    }

    fn all_notes(&self) -> Vec<MemoryEntry> {
        let mut by_text: BTreeMap<String, MemoryEntry> = BTreeMap::new();
        let mut merge = |entry: MemoryEntry| {
            if entry.kind != "note" {
                return;
            }
            match by_text.get(&entry.text) {
                Some(existing) if existing.updated_at >= entry.updated_at => {}
                _ => {
                    by_text.insert(entry.text.clone(), entry);
                }
            }
        };
        for entry in read_memory(&self.memory_path()) {
            merge(entry);
        }
        if let Some(previous) = &self.previous {
            if let Ok(events) = read_events(previous) {
                for entry in memory_entries(&events) {
                    merge(entry);
                }
            }
        }
        if let Ok(events) = read_events(&self.dir) {
            for entry in memory_entries(&events) {
                merge(entry);
            }
        }
        by_text.into_values().collect()
    }

    pub fn notes_section(
        &self,
        paths: &[&str],
        symbols: &[&str],
        heading: &str,
        limit: usize,
    ) -> Option<Section> {
        let mut notes: Vec<MemoryEntry> = self
            .all_notes()
            .into_iter()
            .filter(|note| {
                let path_hit = note.path.as_deref().is_some_and(|note_path| {
                    paths.iter().any(|path| {
                        relative_path(&self.root, note_path) == relative_path(&self.root, path)
                    })
                });
                let symbol_hit = note.symbol.as_deref().is_some_and(|note_symbol| {
                    symbols.contains(&note_symbol)
                });
                path_hit || symbol_hit
            })
            .collect();
        notes.sort_by_key(|note| std::cmp::Reverse(note.updated_at));
        if notes.is_empty() {
            return None;
        }
        let total = notes.len();
        let shown = total.min(limit.max(1));
        let mut body = String::new();
        for note in notes.iter().take(shown) {
            let tags = if note.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", note.tags.join(", "))
            };
            let anchor = match (note.path.as_deref(), note.symbol.as_deref()) {
                (Some(path), Some(symbol)) => format!(" `{path}#{symbol}`"),
                (Some(path), None) => format!(" `{path}`"),
                _ => String::new(),
            };
            let stale = stale_marker(&self.root, note.path.as_deref(), note.anchor_hash.as_deref());
            body.push_str(&format!("- {}{tags}{anchor}{stale}\n", note.text));
        }
        let heading = if shown < total {
            format!("### {heading} — {shown} de {total} nota(s)\n")
        } else {
            format!("### {heading} — {total} nota(s)\n")
        };
        Some(Section {
            heading,
            body,
            priority: 253,
        })
    }

    pub fn record_note(
        &self,
        text: &str,
        tags: Vec<String>,
        path: Option<&str>,
        symbol: Option<&str>,
    ) -> std::io::Result<()> {
        let text = clean(text);
        if text.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "nota vazia",
            ));
        }
        let mut event = StateEvent::new(&self.session, "note");
        event.text = Some(text);
        event.tags = (!tags.is_empty()).then(|| tags.into_iter().map(|t| clean(&t)).collect());
        event.path = path.map(clean);
        event.symbol = symbol.map(clean);
        let anchor = event
            .path
            .as_deref()
            .and_then(|path| file_hash(&self.root, path));
        event.hash = anchor;
        self.record(event)
    }

    pub fn record_task(
        &self,
        text: &str,
        status: &str,
        task_id: Option<&str>,
    ) -> std::io::Result<()> {
        let text = clean(text);
        if text.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tarefa vazia",
            ));
        }
        let id = task_id
            .map(clean)
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| short_hash(&text));
        let mut event = StateEvent::new(&self.session, "task");
        event.task_id = Some(id);
        event.text = Some(text);
        event.status = Some(clean(status));
        self.record(event)
    }

    pub fn finish(&self) {
        let event = StateEvent::new(&self.session, "session_end");
        let _ = self.record(event);
    }

    /// A compact block for the next session. Returns `None` when there is no
    /// previous session and no rolled-up memory.
    pub fn resume(&self) -> Option<String> {
        let memory = read_memory(&self.memory_path());
        let events = self
            .previous
            .as_ref()
            .and_then(|dir| read_events(dir).ok())
            .unwrap_or_default();
        let summary = Summary::from_events(&events, &self.root).merge_memory(memory);
        if summary.actions == 0 && summary.tasks.is_empty() && summary.notes.is_empty() {
            return None;
        }
        let head = self
            .previous
            .as_ref()
            .and_then(|dir| read_meta(dir))
            .and_then(|meta| meta.head);
        let mut body = summary.render_resume(head.as_deref());
        truncate_chars(&mut body, RESUME_MAX_CHARS);
        Some(body)
    }

    pub fn status(&self) -> String {
        let events = read_events(&self.dir).unwrap_or_default();
        let head = read_meta(&self.dir).and_then(|meta| meta.head);
        Summary::from_events(&events, &self.root).render_status(&self.session, head.as_deref())
    }

    fn record(&self, event: StateEvent) -> std::io::Result<()> {
        let _guard = self.append_lock.lock().unwrap_or_else(|e| e.into_inner());
        let first = self.events.load(Ordering::Relaxed) == 0;

        ensure_private_dir(&self.dir)?;
        if first {
            self.write_meta()?;
            self.prune();
        }
        self.rotate_if_needed()?;

        let path = self.dir.join("events.jsonl");
        let mut file = private_append(&path)?;
        let line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
        self.events.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn write_meta(&self) -> std::io::Result<()> {
        let meta = Meta {
            v: 1,
            session: self.session.clone(),
            root: self.root.display().to_string(),
            started_at: now_secs(),
            head: git_head(&self.root),
        };
        let body = serde_json::to_vec(&meta).map_err(std::io::Error::other)?;
        write_private(&self.dir.join("meta.json"), &body)
    }

    fn rotate_if_needed(&self) -> std::io::Result<()> {
        let path = self.dir.join("events.jsonl");
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len <= MAX_EVENT_FILE_BYTES {
            return Ok(());
        }
        let mut index = 1;
        loop {
            let rotated = self.dir.join(format!("events-{index:04}.jsonl"));
            if !rotated.exists() {
                return fs::rename(&path, rotated);
            }
            index += 1;
        }
    }

    fn prune(&self) {
        let sessions = self.root.join(STATE_DIR).join("sessions");
        let Ok(reader) = fs::read_dir(&sessions) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = reader
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        while dirs.len() > MAX_SESSIONS {
            let oldest = dirs.remove(0);
            if oldest != self.dir {
                self.rollup_session(&oldest);
                let _ = fs::remove_dir_all(oldest);
            }
        }
    }

    fn memory_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("memory.jsonl")
    }

    fn rollup_session(&self, dir: &Path) {
        let Ok(events) = read_events(dir) else {
            return;
        };
        let entries = memory_entries(&events);
        if entries.is_empty() {
            return;
        }
        let path = self.memory_path();
        let mut by_key: BTreeMap<String, MemoryEntry> = read_memory(&path)
            .into_iter()
            .map(|entry| (entry.key(), entry))
            .collect();
        for entry in entries {
            let key = entry.key();
            match by_key.get(&key) {
                Some(existing) if existing.updated_at >= entry.updated_at => {}
                _ => {
                    by_key.insert(key, entry);
                }
            }
        }
        let mut entries: Vec<MemoryEntry> = by_key.into_values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        let mut notes = 0usize;
        let mut tasks = 0usize;
        entries.retain(|entry| {
            if entry.kind == "task" {
                tasks += 1;
                tasks <= MAX_MEMORY_TASKS
            } else {
                notes += 1;
                notes <= MAX_MEMORY_NOTES
            }
        });
        entries.sort_by_key(|entry| entry.updated_at);
        write_memory(&path, &entries);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryEntry {
    v: u8,
    kind: String,
    text: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    updated_at: u64,
}

impl MemoryEntry {
    fn key(&self) -> String {
        match self.kind.as_str() {
            "task" => format!("task:{}", self.task_id.as_deref().unwrap_or(&self.text)),
            _ => format!("note:{}", self.text),
        }
    }
}

#[derive(Default)]
struct Summary {
    root: PathBuf,
    session: String,
    last_ts: u64,
    actions: usize,
    working: BTreeMap<(String, String), Working>,
    tasks: BTreeMap<String, TaskItem>,
    notes: Vec<NoteItem>,
    last_tool: Option<(String, Option<String>, bool)>,
    memory_count: usize,
}

#[derive(Default)]
struct Working {
    count: usize,
    last_ts: u64,
}

struct TaskItem {
    text: String,
    status: String,
    last_ts: u64,
}

struct NoteItem {
    text: String,
    tags: Vec<String>,
    path: Option<String>,
    symbol: Option<String>,
    anchor_hash: Option<String>,
    last_ts: u64,
}

impl Summary {
    fn from_events(events: &[StateEvent], root: &Path) -> Self {
        let mut summary = Self {
            root: root.to_path_buf(),
            ..Self::default()
        };
        for event in events {
            summary.last_ts = summary.last_ts.max(event.ts);
            if summary.session.is_empty() {
                summary.session = event.session.clone();
            }

            match event.kind.as_str() {
                "tool" => {
                    summary.actions += 1;
                    if let Some(tool) = &event.tool {
                        summary.last_tool =
                            Some((tool.clone(), event.target.clone(), event.ok.unwrap_or(true)));
                    }
                    if let Some(path) = &event.path {
                        let symbol = event.symbol.clone().unwrap_or_default();
                        let entry = summary.working.entry((path.clone(), symbol)).or_default();
                        entry.count += 1;
                        entry.last_ts = event.ts;
                    }
                }
                "note" => {
                    if let Some(text) = &event.text {
                        summary.notes.retain(|note| note.text != *text);
                        summary.notes.push(NoteItem {
                            text: text.clone(),
                            tags: event.tags.clone().unwrap_or_default(),
                            path: event.path.clone(),
                            symbol: event.symbol.clone(),
                            anchor_hash: event.hash.clone(),
                            last_ts: event.ts,
                        });
                    }
                }
                "task" => {
                    if let Some(text) = &event.text {
                        let id = event
                            .task_id
                            .clone()
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| short_hash(text));
                        summary.tasks.insert(
                            id,
                            TaskItem {
                                text: text.clone(),
                                status: event.status.clone().unwrap_or_else(|| "open".to_string()),
                                last_ts: event.ts,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        summary
    }

    fn merge_memory(mut self, memory: Vec<MemoryEntry>) -> Self {
        self.memory_count = memory.len();
        for entry in memory {
            match entry.kind.as_str() {
                "note" => {
                    if !self.notes.iter().any(|note| note.text == entry.text) {
                        self.notes.push(NoteItem {
                            text: entry.text,
                            tags: entry.tags,
                            path: entry.path,
                            symbol: entry.symbol,
                            anchor_hash: entry.anchor_hash,
                            last_ts: entry.updated_at,
                        });
                    }
                }
                "task" => {
                    let id = entry
                        .task_id
                        .clone()
                        .unwrap_or_else(|| short_hash(&entry.text));
                    self.tasks.entry(id).or_insert(TaskItem {
                        text: entry.text,
                        status: entry.status.unwrap_or_else(|| "open".to_string()),
                        last_ts: entry.updated_at,
                    });
                }
                _ => {}
            }
        }
        self
    }

    fn render_resume(&self, head: Option<&str>) -> String {
        let mut out = String::from("### Estado local (sessão anterior)\n");
        if !self.session.is_empty() {
            let head = head
                .filter(|head| !head.is_empty())
                .map(|head| format!(", HEAD `{head}`"))
                .unwrap_or_default();
            out.push_str(&format!(
                "Sessão `{}` ({}), {} ação(ões){head}.\n",
                self.session,
                human_age(self.last_ts),
                self.actions
            ));
        }
        self.render_last_tool(&mut out);
        self.render_working(&mut out);
        self.render_tasks(&mut out, true);
        self.render_notes(&mut out);
        if self.memory_count > 0 {
            out.push_str(&format!(
                "Memória durável: {} entrada(s); use `brain_state action=memory`.\n",
                self.memory_count
            ));
        }
        out.push_str(
            "Detalhe com `brain_state action=resume`; registre decisões com `action=note`.\n",
        );
        out
    }

    fn render_status(&self, session: &str, head: Option<&str>) -> String {
        let mut out = String::from("### Sessão atual\n");
        if self.actions == 0 && self.tasks.is_empty() && self.notes.is_empty() {
            out.push_str("Sem eventos registrados ainda.\n");
            return out;
        }
        let head = head
            .filter(|head| !head.is_empty())
            .map(|head| format!(", HEAD `{head}`"))
            .unwrap_or_default();
        out.push_str(&format!(
            "Sessão `{}`. {} ação(ões){head}.\n",
            session, self.actions
        ));
        self.render_last_tool(&mut out);
        self.render_working(&mut out);
        self.render_tasks(&mut out, false);
        self.render_notes(&mut out);
        out
    }

    fn render_last_tool(&self, out: &mut String) {
        let Some((tool, target, ok)) = &self.last_tool else {
            return;
        };
        let target = target
            .as_deref()
            .filter(|target| !target.is_empty())
            .map(|target| format!(" ({target})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "Última ação: `{tool}`{target} — {}.\n",
            if *ok { "ok" } else { "falhou" }
        ));
    }

    fn render_working(&self, out: &mut String) {
        let mut working: Vec<(&(String, String), &Working)> = self.working.iter().collect();
        working.sort_by(|(_, a), (_, b)| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_ts.cmp(&a.last_ts))
        });
        if working.is_empty() {
            return;
        }
        let items: Vec<String> = working
            .into_iter()
            .take(6)
            .map(|((path, symbol), entry)| {
                if symbol.is_empty() {
                    format!("`{path}` ×{}", entry.count)
                } else {
                    format!("`{path}` ({symbol} ×{})", entry.count)
                }
            })
            .collect();
        out.push_str(&format!("Trabalhou em: {}.\n", items.join(", ")));
    }

    fn render_tasks(&self, out: &mut String, open_only: bool) {
        let mut tasks: Vec<&TaskItem> = self
            .tasks
            .values()
            .filter(|task| !open_only || task.status != "done")
            .collect();
        tasks.sort_by(|a, b| {
            let rank = |status: &str| if status == "done" { 1 } else { 0 };
            rank(&a.status)
                .cmp(&rank(&b.status))
                .then_with(|| b.last_ts.cmp(&a.last_ts))
        });
        if tasks.is_empty() {
            return;
        }
        let items: Vec<String> = tasks
            .into_iter()
            .take(6)
            .map(|task| {
                if task.status == "done" {
                    format!("{} (concluída)", task.text)
                } else {
                    task.text.clone()
                }
            })
            .collect();
        let label = if open_only {
            "Tarefas abertas"
        } else {
            "Tarefas"
        };
        out.push_str(&format!("{label}: {}.\n", items.join("; ")));
    }

    fn render_notes(&self, out: &mut String) {
        let mut notes: Vec<&NoteItem> = self.notes.iter().collect();
        notes.sort_by_key(|note| std::cmp::Reverse(note.last_ts));
        if notes.is_empty() {
            return;
        }
        out.push_str("Notas:\n");
        for note in notes.into_iter().take(4) {
            let tags = if note.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", note.tags.join(", "))
            };
            let anchor = match (&note.path, &note.symbol) {
                (Some(path), Some(symbol)) => format!(" `{path}#{symbol}`"),
                (Some(path), None) => format!(" `{path}`"),
                _ => String::new(),
            };
            let stale = stale_marker(&self.root, note.path.as_deref(), note.anchor_hash.as_deref());
            out.push_str(&format!("- {}{tags}{anchor}{stale}\n", note.text));
        }
    }
}

fn previous_session(root: &Path, current: &str) -> Option<PathBuf> {
    let sessions = root.join(STATE_DIR).join("sessions");
    let mut dirs: Vec<PathBuf> = fs::read_dir(sessions)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs.into_iter().rev() {
        if dir.file_name().and_then(|name| name.to_str()) == Some(current) {
            continue;
        }
        let has_events = fs::read_dir(&dir).is_ok_and(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("events") && name.ends_with(".jsonl"))
            })
        });
        if has_events {
            return Some(dir);
        }
    }
    None
}

fn read_meta(dir: &Path) -> Option<Meta> {
    let body = fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&body).ok()
}

fn read_events(dir: &Path) -> std::io::Result<Vec<StateEvent>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("events") && name.ends_with(".jsonl"))
        })
        .collect();
    files.sort();

    let mut events = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<StateEvent>(line) {
                events.push(event);
            }
        }
    }
    if events.len() > MAX_EVENTS {
        events.drain(..events.len() - MAX_EVENTS);
    }
    Ok(events)
}

fn memory_entries(events: &[StateEvent]) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "note" => {
                if let Some(text) = &event.text {
                    entries.push(MemoryEntry {
                        v: 1,
                        kind: "note".to_string(),
                        text: text.clone(),
                        tags: event.tags.clone().unwrap_or_default(),
                        path: event.path.clone(),
                        symbol: event.symbol.clone(),
                        anchor_hash: event.hash.clone(),
                        task_id: None,
                        status: None,
                        updated_at: event.ts,
                    });
                }
            }
            "task" => {
                if let Some(text) = &event.text {
                    let id = event
                        .task_id
                        .clone()
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| short_hash(text));
                    entries.push(MemoryEntry {
                        v: 1,
                        kind: "task".to_string(),
                        text: text.clone(),
                        tags: Vec::new(),
                        path: None,
                        symbol: None,
                        anchor_hash: None,
                        task_id: Some(id),
                        status: Some(
                            event.status.clone().unwrap_or_else(|| "open".to_string()),
                        ),
                        updated_at: event.ts,
                    });
                }
            }
            _ => {}
        }
    }
    entries
}

fn read_memory(path: &Path) -> Vec<MemoryEntry> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<MemoryEntry>(line.trim()).ok())
        .collect()
}

fn write_memory(path: &Path, entries: &[MemoryEntry]) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = ensure_private_dir(parent);
    let body = entries
        .iter()
        .filter_map(|entry| serde_json::to_string(entry).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if write_private(&tmp, body.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn private_append(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn relative_path(root: &Path, path: &str) -> String {
    let path = Path::new(path);
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches("./")
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn is_mutating(tool: &str) -> bool {
    matches!(
        tool,
        "brain_edit" | "brain_write" | "brain_write_many" | "brain_move" | "brain_patch"
    )
}

fn stale_marker(root: &Path, path: Option<&str>, expected: Option<&str>) -> &'static str {
    match (path, expected) {
        (Some(path), Some(expected)) => match file_hash(root, path) {
            Some(actual) if actual == expected => "",
            Some(_) => " (âncora mudou)",
            None => " (arquivo ausente)",
        },
        _ => "",
    }
}

fn file_hash(root: &Path, path: &str) -> Option<String> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let bytes = fs::read(absolute).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

fn clean(text: &str) -> String {
    let mut normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&mut normalized, TEXT_MAX_CHARS);
    normalized
}

fn short_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()[..8].to_string()
}

fn truncate_chars(text: &mut String, max: usize) {
    if text.chars().count() <= max {
        return;
    }
    let cut = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(cut);
}

fn human_age(ts: u64) -> String {
    let seconds = now_secs().saturating_sub(ts);
    match seconds {
        0..=59 => "agora".to_string(),
        60..=3599 => format!("há {}min", seconds / 60),
        3600..=86_399 => format!("há {}h", seconds / 3600),
        _ => format!("há {}d", seconds / 86_400),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn memory_text(root: &Path, path: &Path, query: Option<&str>) -> String {
    let mut entries = read_memory(path);
    if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        entries.retain(|entry| {
            let haystack = format!(
                "{} {} {}",
                entry.text,
                entry.tags.join(" "),
                entry.path.as_deref().unwrap_or("")
            )
            .to_lowercase();
            terms.iter().any(|term| haystack.contains(term))
        });
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    if entries.is_empty() {
        return "Memória durável vazia.".to_string();
    }
    let mut out = format!("### Memória durável ({} entrada(s))\n", entries.len());
    for entry in entries.into_iter().take(20) {
        let kind = if entry.kind == "task" { "tarefa" } else { "nota" };
        let status = entry
            .status
            .as_deref()
            .filter(|status| *status != "open")
            .map(|status| format!(" ({status})"))
            .unwrap_or_default();
        let tags = if entry.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", entry.tags.join(", "))
        };
        let anchor = match (entry.path.as_deref(), entry.symbol.as_deref()) {
            (Some(path), Some(symbol)) => format!(" `{path}#{symbol}`"),
            (Some(path), None) => format!(" `{path}`"),
            _ => String::new(),
        };
        let stale = stale_marker(root, entry.path.as_deref(), entry.anchor_hash.as_deref());
        out.push_str(&format!(
            "- {kind}: {}{tags}{anchor}{status}{stale}\n",
            entry.text
        ));
    }
    out
}
impl StateStore {
    pub fn memory(&self, query: Option<&str>) -> String {
        memory_text(&self.root, &self.memory_path(), query)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record(store: &StateStore) {
        store.record_tool(ToolRecord {
            tool: "brain_read",
            target: Some("src/lib.rs"),
            path: Some("src/lib.rs"),
            symbol: Some("verify_token"),
            ok: true,
            ms: 3,
            bytes: 128,
        });
    }

    #[test]
    fn a_previous_session_is_resumed() {
        let dir = tempdir().unwrap();
        let first = StateStore::open(dir.path());
        record(&first);
        first
            .record_note(
                "usar --features x",
                vec!["test".to_string()],
                Some("src/lib.rs"),
                None,
            )
            .unwrap();
        first.record_task("terminar refresh", "open", None).unwrap();
        drop(first);

        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = StateStore::open(dir.path());
        let resume = second.resume().expect("previous session must resume");
        assert!(resume.contains("src/lib.rs"), "{resume}");
        assert!(resume.contains("usar --features x"), "{resume}");
        assert!(resume.contains("terminar refresh"), "{resume}");
    }

    #[test]
    fn the_current_session_is_never_its_own_previous() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        record(&store);
        assert!(store.resume().is_none());
    }

    #[test]
    fn a_corrupt_line_loses_only_itself() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        record(&store);
        let path = store.dir.join("events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{ not json").unwrap();
        record(&store);

        let events = read_events(&store.dir).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn the_latest_task_status_wins() {
        let dir = tempdir().unwrap();
        let first = StateStore::open(dir.path());
        first.record_task("terminar refresh", "open", None).unwrap();
        first.record_task("terminar refresh", "done", None).unwrap();
        drop(first);

        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = StateStore::open(dir.path());
        let resume = second.resume().unwrap();
        assert!(!resume.contains("Tarefas abertas"), "{resume}");
    }

    #[test]
    fn notes_are_deduplicated_by_text() {
        let dir = tempdir().unwrap();
        let first = StateStore::open(dir.path());
        first.record_note("a mesma nota", vec![], None, None).unwrap();
        first.record_note("a mesma nota", vec![], None, None).unwrap();
        drop(first);

        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = StateStore::open(dir.path());
        let resume = second.resume().unwrap();
        assert_eq!(resume.matches("a mesma nota").count(), 1, "{resume}");
    }

    #[test]
    fn old_sessions_roll_up_into_memory() {
        let dir = tempdir().unwrap();
        for i in 0..(MAX_SESSIONS + 2) {
            let store = StateStore::open(dir.path());
            if i == 0 {
                store
                    .record_note("nota antiga", vec![], None, None)
                    .unwrap();
            } else {
                store
                    .record_task(&format!("tarefa {i}"), "done", None)
                    .unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let latest = StateStore::open(dir.path());
        let memory = read_memory(&latest.memory_path());
        assert!(
            memory.iter().any(|entry| entry.text == "nota antiga"),
            "{memory:?}"
        );
        let resume = latest.resume().expect("memory must resume");
        assert!(resume.contains("nota antiga"), "{resume}");
    }

    #[test]
    fn a_changed_anchor_is_marked_stale() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("src.rs");
        fs::write(&file, "fn a() {}\n").unwrap();

        let first = StateStore::open(dir.path());
        first
            .record_note("não usar unwrap", vec![], Some("src.rs"), None)
            .unwrap();
        drop(first);

        fs::write(&file, "fn a() { 1; }\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = StateStore::open(dir.path());
        let resume = second.resume().unwrap();
        assert!(resume.contains("âncora mudou"), "{resume}");
    }

    #[test]
    fn memory_searches_notes_and_tasks() {
        let dir = tempdir().unwrap();
        let path = StateStore::open(dir.path()).memory_path();
        write_memory(
            &path,
            &[
                MemoryEntry {
                    v: 1,
                    kind: "note".to_string(),
                    text: "refresh token".to_string(),
                    tags: Vec::new(),
                    path: None,
                    symbol: None,
                    anchor_hash: None,
                    task_id: None,
                    status: None,
                    updated_at: 2,
                },
                MemoryEntry {
                    v: 1,
                    kind: "task".to_string(),
                    text: "compilar".to_string(),
                    tags: Vec::new(),
                    path: None,
                    symbol: None,
                    anchor_hash: None,
                    task_id: Some("t1".to_string()),
                    status: Some("open".to_string()),
                    updated_at: 1,
                },
            ],
        );
        let store = StateStore::open(dir.path());
        let out = store.memory(Some("refresh"));
        assert!(out.contains("refresh token"), "{out}");
        assert!(!out.contains("compilar"), "{out}");
        assert!(store.memory(None).contains("compilar"));
    }
    #[test]
    fn a_failed_exec_then_edit_then_pass_is_inferred() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        store.record_tool(ToolRecord {
            tool: "brain_exec",
            target: Some("test"),
            path: None,
            symbol: None,
            ok: false,
            ms: 10,
            bytes: 100,
        });
        store.record_tool(ToolRecord {
            tool: "brain_edit",
            target: Some("src/lib.rs"),
            path: Some("src/lib.rs"),
            symbol: Some("verify"),
            ok: true,
            ms: 2,
            bytes: 10,
        });
        store.record_tool(ToolRecord {
            tool: "brain_exec",
            target: Some("test"),
            path: None,
            symbol: None,
            ok: true,
            ms: 8,
            bytes: 80,
        });

        let events = read_events(&store.dir).unwrap();
        let inferred = events
            .iter()
            .find(|event| {
                event.kind == "note"
                    && event
                        .tags
                        .as_ref()
                        .is_some_and(|tags| tags.iter().any(|tag| tag == "inferred"))
            })
            .expect("inference note");
        let text = inferred.text.as_deref().unwrap_or("");
        assert!(text.contains("src/lib.rs"), "{inferred:?}");
        assert!(text.contains("test"), "{inferred:?}");
    }

    #[test]
    fn a_pass_without_an_edit_is_not_inferred() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        store.record_tool(ToolRecord {
            tool: "brain_exec",
            target: Some("test"),
            path: None,
            symbol: None,
            ok: false,
            ms: 10,
            bytes: 100,
        });
        store.record_tool(ToolRecord {
            tool: "brain_exec",
            target: Some("test"),
            path: None,
            symbol: None,
            ok: true,
            ms: 8,
            bytes: 80,
        });

        let events = read_events(&store.dir).unwrap();
        assert!(!events.iter().any(|event| event.kind == "note"));
    }
    #[test]
    fn hot_paths_weight_the_working_set() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        for _ in 0..3 {
            store.record_tool(ToolRecord {
                tool: "brain_read",
                target: Some("src/a.rs"),
                path: Some("src/a.rs"),
                symbol: None,
                ok: true,
                ms: 1,
                bytes: 0,
            });
        }
        store.record_tool(ToolRecord {
            tool: "brain_read",
            target: Some("src/b.rs"),
            path: Some("src/b.rs"),
            symbol: None,
            ok: true,
            ms: 1,
            bytes: 0,
        });

        let hot = store.hot_paths(4);
        let a = hot.get("src/a.rs").copied().unwrap_or(0.0);
        let b = hot.get("src/b.rs").copied().unwrap_or(0.0);
        assert!(a > 0.9, "{hot:?}");
        assert!(b < a, "{hot:?}");
    }
    #[test]
    fn notes_section_matches_paths_and_symbols() {
        let dir = tempdir().unwrap();
        let store = StateStore::open(dir.path());
        store
            .record_note("nota do arquivo", vec![], Some("src/lib.rs"), None)
            .unwrap();
        store
            .record_note("nota do símbolo", vec![], None, Some("verify"))
            .unwrap();
        store
            .record_note("nota alheia", vec![], Some("src/other.rs"), None)
            .unwrap();

        let by_path = store
            .notes_section(&["src/lib.rs"], &[], "Memória", 8)
            .expect("path match");
        assert!(by_path.body.contains("nota do arquivo"), "{}", by_path.body);
        assert!(!by_path.body.contains("nota alheia"), "{}", by_path.body);

        let by_symbol = store
            .notes_section(&[], &["verify"], "Memória", 8)
            .expect("symbol match");
        assert!(by_symbol.body.contains("nota do símbolo"), "{}", by_symbol.body);
        assert!(!by_symbol.body.contains("nota alheia"), "{}", by_symbol.body);
    }
}
