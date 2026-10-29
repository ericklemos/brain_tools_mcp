use brain_web::search::{canonical_url, fuse, host_of, Hit};

fn hit(title: &str, url: &str, snippet: &str, rank: usize) -> Hit {
    Hit {
        title: title.to_string(),
        url: url.to_string(),
        snippet: snippet.to_string(),
        provider: "test",
        rank,
    }
}

#[test]
fn canonicalization_drops_the_noise_that_duplicates_results() {
    assert_eq!(
        canonical_url("https://www.Example.com/docs/?utm_source=x&page=2#install"),
        "https://example.com/docs?page=2"
    );
    assert_eq!(
        canonical_url("https://example.com/docs/"),
        "https://example.com/docs"
    );
    assert_eq!(host_of("https://www.example.com/a/b"), "example.com");
}

#[test]
fn the_same_page_from_two_providers_becomes_one_hit() {
    let results = vec![
        (
            "alpha",
            vec![hit("Docs", "https://example.com/docs", "short", 0)],
        ),
        (
            "beta",
            vec![hit(
                "Docs",
                "https://www.example.com/docs/?utm_campaign=x",
                "a much longer snippet",
                0,
            )],
        ),
    ];

    let fused = fuse(results, 10);
    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].snippet, "a much longer snippet");
}

#[test]
fn consensus_outranks_a_single_first_place() {
    let results = vec![
        ("alpha", vec![hit("Only", "https://a.example/1", "", 0)]),
        (
            "beta",
            vec![hit("Shared", "https://shared.example/2", "", 1)],
        ),
        (
            "gamma",
            vec![hit("Shared", "https://shared.example/2", "", 1)],
        ),
    ];

    let fused = fuse(results, 10);
    assert_eq!(fused[0].url, "https://shared.example/2");
}

#[test]
fn syndicated_copies_with_the_same_title_and_host_are_collapsed() {
    let results = vec![(
        "alpha",
        vec![
            hit("Same Story", "https://news.example/a", "", 0),
            hit("Same Story", "https://news.example/b", "", 1),
        ],
    )];

    let fused = fuse(results, 10);
    assert_eq!(fused.len(), 1);
}

#[test]
fn the_limit_is_applied_after_fusion() {
    let results = vec![(
        "alpha",
        (0..10)
            .map(|index| hit(&format!("Page {index}"), &format!("https://e.example/{index}"), "", index))
            .collect(),
    )];
    assert_eq!(fuse(results, 3).len(), 3);
}
