//! Local, append-only usage log for `brain_shell`.
//!
//! The point is to learn which commands the agent actually reaches for, so a
//! dedicated tool (`brain_git`, `brain_cargo`, ...) can be justified by data
//! instead of intuition. The log is metadata-first: commands are redacted and
//! stdout/stderr are reduced to byte counts unless the caller explicitly opts
//! into `--shell-usage-log-full`.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate the usage log once it reaches this size. The previous file stays
/// next to it as `shell-usage.jsonl.1`.
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// One line of `.brain/shell-usage.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellUsageEntry {
    /// Unix epoch in milliseconds.
    pub ts_ms: u64,
    /// Process id of the MCP daemon that ran the command.
    pub pid: u32,
    /// Project root the command ran in.
    pub cwd: String,
    /// Command with common secret patterns redacted. Always present.
    pub command: String,
    /// Raw command. Only written when the log was created with `full = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_raw: Option<String>,
    /// First whitespace-delimited token, for cheap aggregation.
    pub program: String,
    /// `ok`, `exit_nonzero`, `signaled`, `timeout`, or `error`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub timeout_secs: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// First ~200 chars of stdout. Only written in full mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_head: Option<String>,
    /// First ~200 chars of stderr. Only written in full mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_head: Option<String>,
    /// Set for validation or spawn failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Writes shell usage entries as JSONL. The file is local-only and private.
pub struct ShellUsageLog {
    path: PathBuf,
    full: bool,
    max_bytes: u64,
    write_lock: Mutex<()>,
}

impl ShellUsageLog {
    pub fn new(path: impl Into<PathBuf>, full: bool) -> Self {
        Self {
            path: path.into(),
            full,
            max_bytes: DEFAULT_MAX_BYTES,
            write_lock: Mutex::new(()),
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn full(&self) -> bool {
        self.full
    }

    /// Appends one JSON line. A logging failure must never fail the command,
    /// so callers are expected to treat the error as best-effort telemetry.
    pub fn record(&self, entry: &ShellUsageEntry) -> std::io::Result<()> {
        // Concurrent brain_shell calls may write at the same time; the lock
        // also keeps a rotation from racing an append.
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        if self.max_bytes > 0 {
            if let Ok(metadata) = fs::metadata(&self.path) {
                if metadata.len() >= self.max_bytes {
                    // Append `.1` instead of replacing the extension, so a
                    // custom path such as `usage.log` becomes `usage.log.1`.
                    let mut rotated = self.path.clone().into_os_string();
                    rotated.push(".1");
                    let _ = fs::rename(&self.path, PathBuf::from(rotated));
                }
            }
        }

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options.open(&self.path)?;
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // `mode` only applies when the file is created; keep an existing
            // log private too.
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }

        Ok(())
    }

    /// Removes the most common secret shapes from a command before logging it.
    ///
    /// This is deliberately best-effort: `--shell-usage-log-full` exists for
    /// the cases where the raw command is needed, and it is opt-in.
    pub fn redact(command: &str) -> String {
        let mut redacted = command.to_string();
        for (pattern, replacement) in redaction_patterns() {
            redacted = pattern.replace_all(&redacted, *replacement).into_owned();
        }
        redacted
    }
}

fn redaction_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // https://user:password@host/...
            (
                Regex::new(r"(?i)(https?://)[^/\s:@]+:[^/\s@]+@").expect("valid redaction regex"),
                "${1}<redacted>@",
            ),
            // Authorization: Bearer <token>
            (
                Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+").expect("valid redaction regex"),
                "${1}<redacted>",
            ),
            // Authorization: Basic <base64>
            (
                Regex::new(r"(?i)(basic\s+)[A-Za-z0-9+/=]+").expect("valid redaction regex"),
                "${1}<redacted>",
            ),
            // KEY=value, TOKEN: value, Authorization: ...
            (
                Regex::new(
                    r#"(?i)([A-Za-z0-9_-]*(?:token|password|passwd|secret|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret|authorization)[A-Za-z0-9_-]*\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"']+)"#,
                )
                .expect("valid redaction regex"),
                "${1}<redacted>",
            ),
            // --token value, --password value, ...
            (
                Regex::new(
                    r#"(?i)(--?[A-Za-z0-9_-]*(?:token|password|passwd|secret|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret)[A-Za-z0-9_-]*\s+)("[^"]*"|'[^']*'|[^\s"']+)"#,
                )
                .expect("valid redaction regex"),
                "${1}<redacted>",
            ),
            // ?token=value, &key=value
            (
                Regex::new(r"(?i)([?&](?:token|key|secret|password)=)[^&\s]+")
                    .expect("valid redaction regex"),
                "${1}<redacted>",
            ),
        ]
    })
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry() -> ShellUsageEntry {
        ShellUsageEntry {
            ts_ms: now_millis(),
            pid: 42,
            cwd: "/tmp/project".to_string(),
            command: "git status".to_string(),
            command_raw: None,
            program: "git".to_string(),
            status: "ok".to_string(),
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 12,
            timeout_secs: 120,
            stdout_bytes: 7,
            stderr_bytes: 0,
            stdout_head: None,
            stderr_head: None,
            error: None,
        }
    }

    #[test]
    fn redacts_common_secret_shapes() {
        let cases = [
            ("export GITHUB_TOKEN=ghp_123", "ghp_123"),
            (
                "curl -H \"Authorization: Bearer abc.def\" https://example.com",
                "abc.def",
            ),
            (
                "git remote set-url origin https://user:pass@example.com/x",
                "pass",
            ),
            ("mysql --password hunter2", "hunter2"),
            ("deploy --api-key=xyz", "xyz"),
            ("curl 'https://example.com?access_token=secret'", "secret"),
        ];

        for (command, secret) in cases {
            let redacted = ShellUsageLog::redact(command);
            assert!(
                !redacted.contains(secret),
                "leaked {secret:?} in {redacted:?}"
            );
            assert!(
                redacted.contains("<redacted>"),
                "did not redact {command:?}"
            );
        }
    }

    #[test]
    fn leaves_ordinary_commands_alone() {
        assert_eq!(
            ShellUsageLog::redact("cargo test --lib"),
            "cargo test --lib"
        );
    }

    #[test]
    fn a_recorded_entry_is_one_json_line() {
        let dir = tempdir().unwrap();
        let log = ShellUsageLog::new(dir.path().join("usage.jsonl"), false);
        log.record(&entry()).unwrap();

        let content = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(content.lines().count(), 1);

        let decoded: ShellUsageEntry = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(decoded.command, "git status");
        assert_eq!(decoded.status, "ok");
        assert_eq!(decoded.exit_code, Some(0));
        assert!(decoded.command_raw.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let log = ShellUsageLog::new(dir.path().join("usage.jsonl"), false);
        log.record(&entry()).unwrap();

        let mode = std::fs::metadata(log.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn rotates_at_the_byte_ceiling() {
        let dir = tempdir().unwrap();
        let log = ShellUsageLog::new(dir.path().join("usage.jsonl"), false).with_max_bytes(1);

        log.record(&entry()).unwrap();
        log.record(&entry()).unwrap();

        assert!(dir.path().join("usage.jsonl.1").exists());
        let content = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(content.lines().count(), 1);
    }
}
