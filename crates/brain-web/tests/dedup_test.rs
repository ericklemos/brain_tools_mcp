use brain_web::{extract, BlockKind};

#[test]
fn a_paragraph_that_echoes_its_heading_is_not_evidence() {
    let html = "<html><body><article>\
        <h2>Project Setup</h2>\
        <p>Project Setup</p>\
        <p>Create the directory and move into it before running the command.</p>\
        </article></body></html>";

    let page = extract::extract(html, "https://example.com/setup").unwrap();
    let echo = page
        .blocks
        .iter()
        .find(|block| block.kind == BlockKind::Paragraph && block.text == "Project Setup");
    assert!(echo.is_none(), "heading echo leaked: {:?}", page.blocks);
}

#[test]
fn identical_blocks_keep_a_single_anchor() {
    let body = "<p>Restart the daemon after editing the configuration file.</p>";
    let html = format!(
        "<html><body><article><h2>Restart</h2>{body}{body}{body}\
         <p>Then confirm the health endpoint answers.</p></article></body></html>"
    );

    let page = extract::extract(&html, "https://example.com/restart").unwrap();
    let repeated = page
        .blocks
        .iter()
        .filter(|block| block.text.starts_with("Restart the daemon"))
        .count();
    assert_eq!(repeated, 1, "blocks: {:?}", page.blocks);

    let mut ids: Vec<&str> = page.blocks.iter().map(|block| block.id.as_str()).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "ids must be unique");
}
