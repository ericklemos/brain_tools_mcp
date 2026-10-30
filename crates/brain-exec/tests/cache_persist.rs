use brain_exec::cache::{BuildCache, Entry};
use brain_exec::runner::Kind;
use brain_kernel::budget::Section;
use std::time::Duration;

fn entry(body: &str) -> Entry {
    Entry {
        key: String::new(),
        sections: vec![Section {
            heading: "### check".to_string(),
            body: body.to_string(),
            priority: 200,
        }],
        success: true,
        duration: Duration::from_millis(1),
    }
}

#[tokio::test]
async fn a_persisted_entry_survives_a_new_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exec.json");

    let cache = BuildCache::new().with_path(&path);
    let (first, from_cache) = cache
        .get_or_run(Kind::Check, dir.path(), None, false, || async {
            entry("cached body")
        })
        .await;
    assert!(!from_cache);
    assert_eq!(first.sections[0].body, "cached body");
    assert!(path.exists(), "the cache must be written to disk");

    let reloaded = BuildCache::new().with_path(&path);
    let (second, from_cache) = reloaded
        .get_or_run(Kind::Check, dir.path(), None, false, || async {
            entry("fresh body")
        })
        .await;
    assert!(from_cache, "the second process must reuse the entry");
    assert_eq!(second.sections[0].body, "cached body");
    assert_eq!(second.duration, Duration::from_millis(1));
}
