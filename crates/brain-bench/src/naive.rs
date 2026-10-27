//! The counterfactual: what the same question costs without the server.
//!
//! Without `brain-tools` an agent opens files with `Read`, which returns the
//! whole file. So the baseline for a task is the byte count (and estimated
//! token count) of the declared files, verbatim from disk.

use anyhow::{bail, Context, Result};
use brain_kernel::budget::estimate_tokens;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NaiveFile {
    pub path: String,
    pub bytes: usize,
    pub tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NaiveCost {
    /// Sorted by path, deduplicated: two patterns naming the same file do not
    /// pay for it twice.
    pub files: Vec<NaiveFile>,
    pub bytes: usize,
    pub tokens: usize,
}

impl NaiveCost {
    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }
}

/// Expands `entries` (relative to `root`) into files and sums their content.
///
/// A missing entry is an error, never a silent zero: a typo in the task file
/// would otherwise inflate every saving ratio in the report.
pub fn measure(root: &Path, entries: &[String]) -> Result<NaiveCost> {
    let mut files: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for entry in entries {
        let relative = sanitize(entry)?;
        let absolute = root.join(&relative);
        if !absolute.exists() {
            bail!("entrada naïve não existe sob {}: {}", root.display(), entry);
        }

        if absolute.is_dir() {
            for file in walk_dir(&absolute)? {
                insert(&mut files, root, &file)?;
            }
        } else {
            insert(&mut files, root, &absolute)?;
        }
    }

    let files: Vec<NaiveFile> = files
        .into_iter()
        .map(|(path, (bytes, tokens))| NaiveFile {
            path,
            bytes,
            tokens,
        })
        .collect();

    Ok(NaiveCost {
        bytes: files.iter().map(|f| f.bytes).sum(),
        tokens: files.iter().map(|f| f.tokens).sum(),
        files,
    })
}

/// Rejects absolute paths and `..` — the same traversal the `brain_read` tool
/// refuses. The benchmark runs against a repository, not the whole machine.
fn sanitize(entry: &str) -> Result<PathBuf> {
    let path = PathBuf::from(entry);
    if path.is_absolute() {
        bail!("entrada naïve absoluta não é permitida: {entry}");
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("entrada naïve com `..` não é permitida: {entry}");
    }
    Ok(path)
}

fn walk_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in ignore::WalkBuilder::new(dir).build() {
        let entry = entry.with_context(|| format!("falha ao varrer {}", dir.display()))?;
        let path = entry.path();
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            found.push(path.to_path_buf());
        }
    }
    found.sort();
    Ok(found)
}

fn insert(files: &mut BTreeMap<String, (usize, usize)>, root: &Path, file: &Path) -> Result<()> {
    let relative = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    if files.contains_key(&relative) {
        return Ok(());
    }
    let bytes = std::fs::read(file).with_context(|| format!("falha ao ler {}", file.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    files.insert(relative, (bytes.len(), estimate_tokens(&text)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("src/inner/b.rs"), "fn b() { let x = 1; }\n").unwrap();
        dir
    }

    #[test]
    fn a_file_costs_its_own_bytes() {
        let dir = fixture();
        let cost = measure(dir.path(), &["src/a.rs".to_string()]).unwrap();
        assert_eq!(cost.files.len(), 1);
        assert_eq!(cost.bytes, "fn a() {}\n".len());
        assert_eq!(cost.tokens, estimate_tokens("fn a() {}\n"));
    }

    #[test]
    fn a_directory_counts_every_file_inside_it() {
        let dir = fixture();
        let cost = measure(dir.path(), &["src".to_string()]).unwrap();
        assert_eq!(cost.paths(), vec!["src/a.rs", "src/inner/b.rs"]);
    }

    #[test]
    fn overlapping_entries_are_not_paid_twice() {
        let dir = fixture();
        let cost = measure(dir.path(), &["src".to_string(), "src/a.rs".to_string()]).unwrap();
        assert_eq!(cost.files.len(), 2);
    }

    #[test]
    fn a_missing_entry_is_an_error_not_a_zero() {
        let dir = fixture();
        let err = measure(dir.path(), &["src/nao-existe.rs".to_string()]).unwrap_err();
        assert!(err.to_string().contains("não existe"), "{err}");
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        let dir = fixture();
        for entry in ["../etc/passwd", "/etc/passwd"] {
            let err = measure(dir.path(), &[entry.to_string()]).unwrap_err();
            assert!(err.to_string().contains("não é permitida"), "{err}");
        }
    }
}
