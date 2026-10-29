//! Measures the search hot path: reparse-per-file (the old behaviour) versus
//! the shared parse cache, serial and parallel.
//!
//! The numbers land in `target/parse_cache_bench.txt`; the test only asserts
//! that both paths produce the same snippets, so it is safe in CI.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use brain_kernel::ast::AstChunker;
use brain_kernel::parse_cache::{ParseCache, ParseWorker};

fn collect_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                out.push((path, content));
            }
        }
    }
}

fn all_lines(content: &str) -> Vec<usize> {
    (1..=content.lines().count()).collect()
}

/// Old behaviour: a fresh parser per file, serial.
fn reparse_per_file(chunker: &AstChunker, files: &[(PathBuf, String)]) -> usize {
    let mut snippets = 0;
    for (path, content) in files {
        let lines = all_lines(content);
        snippets += chunker
            .extract_snippets(path, content, &lines)
            .expect("extract")
            .len();
    }
    snippets
}

/// New behaviour: one parser per worker, cache hits after the first pass.
fn serial_with_cache(chunker: &AstChunker, files: &[(PathBuf, String)]) -> usize {
    let mut worker = ParseWorker::new();
    let mut snippets = 0;
    for (path, content) in files {
        let lines = all_lines(content);
        snippets += chunker
            .extract_snippets_with(&mut worker, path, content, &lines)
            .expect("extract")
            .len();
    }
    snippets
}

fn parallel_with_cache(chunker: &AstChunker, files: &[(PathBuf, String)]) -> usize {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(files.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut snippets = 0;
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let chunker = chunker;
            let files = &files;
            let next = &next;
            handles.push(scope.spawn(move || {
                let mut worker = ParseWorker::new();
                let mut snippets = 0;
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((path, content)) = files.get(index) else {
                        break;
                    };
                    let lines = all_lines(content);
                    snippets += chunker
                        .extract_snippets_with(&mut worker, path, content, &lines)
                        .expect("extract")
                        .len();
                }
                snippets
            }));
        }
        for handle in handles {
            snippets += handle.join().expect("worker");
        }
    });
    snippets
}

fn best_of<F: FnMut() -> usize>(mut run: F) -> (u128, usize) {
    let mut best = u128::MAX;
    let mut snippets = 0;
    for _ in 0..3 {
        let started = Instant::now();
        snippets = run();
        best = best.min(started.elapsed().as_micros());
    }
    (best, snippets)
}

#[test]
fn cached_and_parallel_extraction_match_the_reparse_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    collect_sources(&root.join("crates"), &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.len() < 5 {
        return;
    }

    let plain = AstChunker::new();
    let (baseline_us, baseline_snippets) = best_of(|| reparse_per_file(&plain, &files));

    // Warm the shared cache the way `build_graph` does, then measure hits.
    let cache = Arc::new(ParseCache::new());
    let cached = AstChunker::with_cache(cache);
    let _ = serial_with_cache(&cached, &files);
    let (cache_us, cache_snippets) = best_of(|| serial_with_cache(&cached, &files));
    let (parallel_us, parallel_snippets) = best_of(|| parallel_with_cache(&cached, &files));

    assert_eq!(baseline_snippets, cache_snippets);
    assert_eq!(baseline_snippets, parallel_snippets);

    let lines: usize = files
        .iter()
        .map(|(_, content)| content.lines().count())
        .sum();
    let report = format!(
        "files={} lines={} snippets={}\n\
         baseline_reparse_us={}\n\
         cache_serial_us={}\n\
         cache_parallel_us={}\n\
         speedup_cache={:.2}x\n\
         speedup_parallel={:.2}x\n",
        files.len(),
        lines,
        baseline_snippets,
        baseline_us,
        cache_us,
        parallel_us,
        baseline_us as f64 / cache_us.max(1) as f64,
        baseline_us as f64 / parallel_us.max(1) as f64,
    );

    let out = root.join("target/parse_cache_bench.txt");
    std::fs::create_dir_all(out.parent().expect("target dir")).expect("mkdir target");
    std::fs::write(out, report).expect("write report");
}
