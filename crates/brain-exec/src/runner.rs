//! Spawns `cargo` and captures its output.
//!
//! The only place in the workspace that starts a process. Kept private to the
//! crate until a second consumer needs it: generalizing now would put
//! `std::process` into the kernel for a single caller.

use brain_kernel::{BrainError, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Build,
    Check,
    Test,
    Clippy,
}

impl Kind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "build" => Some(Kind::Build),
            "check" => Some(Kind::Check),
            "test" => Some(Kind::Test),
            "clippy" => Some(Kind::Clippy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Build => "build",
            Kind::Check => "check",
            Kind::Test => "test",
            Kind::Clippy => "clippy",
        }
    }

    /// The cargo subcommand plus flags. There is no code path that accepts a
    /// caller-supplied command: the surface is these four values.
    ///
    /// `--message-format=json` is always on — including for `test`, where cargo
    /// writes JSON build records and the libtest text to the same stream, so one
    /// run reports compile errors and test failures together.
    fn args(self, manifest: &Path) -> Vec<String> {
        let mut args = vec![
            self.as_str().to_string(),
            "--message-format=json".to_string(),
            "--color=never".to_string(),
            "--manifest-path".to_string(),
            manifest.display().to_string(),
        ];
        if self == Kind::Test {
            // Stopping at the first failing binary hides the rest and makes the
            // agent discover failures one call at a time.
            args.push("--no-fail-fast".to_string());
        }
        args
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
}

impl Outcome {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

pub struct Runner {
    root_dir: PathBuf,
    timeout: Duration,
}

impl Runner {
    pub fn new(root_dir: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            root_dir: root_dir.into(),
            timeout,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root_dir
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root_dir.join("Cargo.toml")
    }

    /// Runs the operation, killing it and returning the partial output on timeout.
    pub async fn run(&self, kind: Kind) -> Result<Outcome> {
        let manifest = self.manifest_path();
        if !manifest.is_file() {
            return Err(BrainError::Config(format!(
                "Cargo.toml não encontrado em {} — brain_exec só opera em raiz de crate/workspace.",
                manifest.display()
            )));
        }

        let started = std::time::Instant::now();
        let mut child = Command::new("cargo")
            .args(kind.args(&manifest))
            .current_dir(&self.root_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BrainError::Config(format!("Falha ao executar cargo: {e}")))?;

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let read_out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let read_err = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let wait = tokio::time::timeout(self.timeout, child.wait()).await;

        let (exit_code, timed_out) = match wait {
            Ok(status) => (status.ok().and_then(|s| s.code()), false),
            Err(_) => {
                // Kill rather than abandon: a running rustc keeps the target/
                // lock and the next call would hang with no explanation.
                let _ = child.kill().await;
                (None, true)
            }
        };

        let stdout = read_out.await.unwrap_or_default();
        let stderr = read_err.await.unwrap_or_default();

        Ok(Outcome {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code,
            timed_out,
            duration: started.elapsed(),
        })
    }
}
