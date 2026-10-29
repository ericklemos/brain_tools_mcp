use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{FailureKind, Result, WebError};
use crate::model::Page;
use crate::util::now_secs;

/// What a cached fetch knows about itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedPage {
    pub url: String,
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: u64,
    pub content_hash: String,
    pub page: Page,
}

impl CachedPage {
    pub fn age(&self) -> Duration {
        Duration::from_secs(now_secs().saturating_sub(self.fetched_at))
    }
}

/// Raw bytes and extracted blocks live side by side: changing the extractor
/// must never mean downloading the page again.
#[derive(Debug, Clone)]
pub struct WebCache {
    root: PathBuf,
}

impl WebCache {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            root: workspace_root.into().join(".brain").join("web"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load(&self, url: &str) -> Option<CachedPage> {
        let dir = self.dir_for(url);
        let meta = fs::read(dir.join("meta.json")).ok()?;
        let page = fs::read(dir.join("page.json")).ok()?;
        let meta: CachedPage = serde_json::from_slice(&meta).ok()?;
        let page: Page = serde_json::from_slice(&page).ok()?;
        if meta.url != url {
            return None;
        }
        Some(CachedPage { page, ..meta })
    }

    pub fn store(&self, entry: &CachedPage, raw: &[u8]) -> Result<()> {
        let dir = self.dir_for(&entry.url);
        fs::create_dir_all(&dir).map_err(|error| {
            WebError::new(FailureKind::Cache, format!("cannot create cache dir: {error}"))
        })?;

        let meta = serde_json::to_vec(entry).map_err(|error| {
            WebError::new(FailureKind::Cache, format!("cannot encode cache meta: {error}"))
        })?;
        let page = serde_json::to_vec(&entry.page).map_err(|error| {
            WebError::new(FailureKind::Cache, format!("cannot encode cached page: {error}"))
        })?;

        atomic_write(&dir.join("raw.bin"), raw)?;
        atomic_write(&dir.join("page.json"), &page)?;
        atomic_write(&dir.join("meta.json"), &meta)?;
        Ok(())
    }

    /// A 304 means the bytes did not change, so only the clock moves.
    pub fn touch(&self, url: &str) -> Result<()> {
        let dir = self.dir_for(url);
        let Ok(bytes) = fs::read(dir.join("meta.json")) else {
            return Ok(());
        };
        let Ok(mut entry) = serde_json::from_slice::<CachedPage>(&bytes) else {
            return Ok(());
        };
        entry.fetched_at = now_secs();
        let encoded = serde_json::to_vec(&entry).map_err(|error| {
            WebError::new(FailureKind::Cache, format!("cannot encode cache meta: {error}"))
        })?;
        atomic_write(&dir.join("meta.json"), &encoded)
    }

    fn dir_for(&self, url: &str) -> PathBuf {
        let key = blake3::hash(normalize_url(url).as_bytes()).to_hex();
        self.root.join(&key.as_str()[..32])
    }
}

/// Fragments do not change the response, so they must not split the cache.
pub fn normalize_url(url: &str) -> String {
    match url.split_once('#') {
        Some((base, _)) => base.to_string(),
        None => url.to_string(),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|error| {
        WebError::new(
            FailureKind::Cache,
            format!("cannot write {}: {error}", tmp.display()),
        )
    })?;
    fs::rename(&tmp, path).map_err(|error| {
        WebError::new(
            FailureKind::Cache,
            format!("cannot commit {}: {error}", path.display()),
        )
    })
}
