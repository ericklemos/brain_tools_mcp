use brain_kernel::{BrainError, Result};
use grep_regex::RegexMatcher;
use grep_searcher::{SearcherBuilder, Sink, SinkMatch};
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMatch {
    pub file_path: PathBuf,
    pub line_number: usize,
    pub byte_offset: u64,
    pub line_content: String,
}

pub struct LexicalScanner {
    root_dir: PathBuf,
}

impl LexicalScanner {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn search_keywords(&self, keywords: &[&str]) -> Result<Vec<RawMatch>> {
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        let mut types_builder = TypesBuilder::new();
        types_builder
            .add("code", "*.rs")
            .map_err(|e| BrainError::Search(e.to_string()))?;
        types_builder
            .add("code", "*.kt")
            .map_err(|e| BrainError::Search(e.to_string()))?;
        types_builder.select("code");
        let types = types_builder
            .build()
            .map_err(|e| BrainError::Search(e.to_string()))?;

        // Build regex pattern matching any of the keywords
        let escaped_keywords: Vec<String> = keywords
            .iter()
            .filter(|k| !k.trim().is_empty())
            .map(|k| regex::escape(k))
            .collect();

        if escaped_keywords.is_empty() {
            return Ok(Vec::new());
        }

        let pattern = escaped_keywords.join("|");
        let matcher = std::sync::Arc::new(
            RegexMatcher::new(&pattern)
                .map_err(|e| BrainError::Search(format!("Invalid regex pattern: {e}")))?,
        );

        let mut walker = WalkBuilder::new(&self.root_dir);
        walker
            .types(types)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);

        // Walking the tree is the hot path of every search. Each worker gets
        // its own Searcher (it owns reusable buffers and is not Sync); only the
        // result vector is shared, and its lock is held for a short extend per
        // file.
        let collected: std::sync::Mutex<Vec<RawMatch>> = std::sync::Mutex::new(Vec::new());

        walker.build_parallel().run(|| {
            let matcher = matcher.clone();
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .multi_line(false)
                .build();
            let collected = &collected;

            Box::new(move |result| {
                let entry = match result {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::debug!(error = %err, "Error reading directory entry");
                        return ignore::WalkState::Continue;
                    }
                };

                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return ignore::WalkState::Continue;
                }

                let path = entry.path();
                // Components, not a formatted string: no allocation per entry.
                let excluded = path.components().any(|component| {
                    matches!(
                        component.as_os_str().to_str(),
                        Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                    )
                });
                if excluded {
                    return ignore::WalkState::Continue;
                }

                let mut file_matches = Vec::new();

                struct MatchCollector<'a> {
                    matches: &'a mut Vec<RawMatch>,
                    file_path: &'a Path,
                }

                impl<'a> Sink for MatchCollector<'a> {
                    type Error = std::io::Error;

                    fn matched(
                        &mut self,
                        _searcher: &grep_searcher::Searcher,
                        mat: &SinkMatch<'_>,
                    ) -> std::result::Result<bool, Self::Error> {
                        let line_number = mat.line_number().unwrap_or(1) as usize;
                        let line_content = String::from_utf8_lossy(mat.bytes()).to_string();
                        let byte_offset = mat.absolute_byte_offset();

                        self.matches.push(RawMatch {
                            file_path: self.file_path.to_path_buf(),
                            line_number,
                            byte_offset,
                            line_content,
                        });

                        Ok(true)
                    }
                }

                let sink = MatchCollector {
                    matches: &mut file_matches,
                    file_path: path,
                };

                if let Err(err) = searcher.search_path(&*matcher, path, sink) {
                    tracing::debug!(path = %path.display(), error = %err, "Error searching file");
                }

                if !file_matches.is_empty() {
                    collected
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .extend(file_matches);
                }

                ignore::WalkState::Continue
            })
        });

        let mut matches = collected
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Parallel walking has no stable order; sorting keeps brain_search
        // (and the bench baseline) reproducible for the same tree.
        matches.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then_with(|| a.line_number.cmp(&b.line_number))
        });

        Ok(matches)
    }
}
