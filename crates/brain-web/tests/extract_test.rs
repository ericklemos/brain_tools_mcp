use brain_web::extract;
use brain_web::FailureKind;

fn page(body: &str) -> String {
    format!(
        "<!doctype html><html><head><title>Doc</title></head><body>\
         <nav><p>Products Pricing Blog</p></nav>\
         <header><p>Sign in</p></header>\
         <article>{body}</article>\
         <footer><p>Copyright 2026 Example Inc</p></footer>\
         </body></html>"
    )
}

#[test]
fn chrome_is_dropped_and_the_body_survives() {
    let html = page(
        "<h1>Installing</h1>\
         <p>Run the installer and wait for the prompt to finish before continuing.</p>",
    );
    let page = extract::extract(&html, "https://example.com/install").unwrap();
    let text = page
        .blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("Run the installer"), "{text}");
    assert!(!text.contains("Pricing"), "navigation leaked: {text}");
    assert!(!text.contains("Copyright"), "footer leaked: {text}");
}

#[test]
fn every_block_carries_its_heading_path() {
    let html = page("<h2>Configuration</h2><p>Set the proxy variable before starting.</p>");
    let page = extract::extract(&html, "https://example.com/config").unwrap();
    let paragraph = page
        .blocks
        .iter()
        .find(|block| block.text.contains("proxy variable"))
        .expect("paragraph block");
    assert_eq!(paragraph.heading, "Configuration");
}

#[test]
fn block_ids_are_stable_across_runs() {
    let html = page("<h2>Usage</h2><p>Call the function with a string argument.</p>");
    let first = extract::extract(&html, "https://example.com/usage").unwrap();
    let second = extract::extract(&html, "https://example.com/usage").unwrap();
    let ids = |page: &brain_web::Page| page.blocks.iter().map(|b| b.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first), ids(&second));
    assert!(ids(&first).iter().all(|id| id.starts_with("blk_")));
}

#[test]
fn a_javascript_shell_is_a_typed_error() {
    let html = "<html><body><div id=\"root\"></div><script>app()</script></body></html>";
    let error = extract::extract(html, "https://example.com/app").unwrap_err();
    assert_eq!(error.kind, FailureKind::JsOnly);
    assert!(error.kind.hint().contains("JavaScript"));
}

#[test]
fn a_table_keeps_its_rows() {
    let html = page(
        "<h2>Limits</h2><table><tr><th>Plan</th><th>Requests</th></tr>\
         <tr><td>Free</td><td>100</td></tr><tr><td>Pro</td><td>10000</td></tr></table>",
    );
    let page = extract::extract(&html, "https://example.com/limits").unwrap();
    let dump = page
        .blocks
        .iter()
        .map(|block| format!("{:?}: {}", block.kind, block.text))
        .collect::<Vec<_>>()
        .join("\n---\n");
    let table = page
        .blocks
        .iter()
        .find(|block| block.kind == brain_web::BlockKind::Table)
        .unwrap_or_else(|| panic!("table block missing; blocks:\n{dump}"));
    assert!(table.text.contains("Free | 100"), "{}", table.text);
    assert!(table.text.contains("Pro | 10000"), "{}", table.text);
}
