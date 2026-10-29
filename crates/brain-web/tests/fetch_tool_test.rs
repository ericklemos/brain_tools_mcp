use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brain_kernel::store::ResponseStore;
use brain_web::http::{FetchRequest, FetchResponse, Fetcher};
use brain_web::{FetchTool, WebError};
use serde_json::json;

struct Fake {
    calls: AtomicUsize,
    responses: Mutex<Vec<FetchResponse>>,
}

impl Fake {
    fn new(responses: Vec<FetchResponse>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            responses: Mutex::new(responses),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Fetcher for Fake {
    fn fetch(
        &self,
        _request: FetchRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FetchResponse, WebError>> + Send + '_>,
    > {
        let response = {
            let mut queue = self.responses.lock().unwrap();
            if queue.is_empty() {
                return Box::pin(async {
                    Err(WebError::new(
                        brain_web::FailureKind::Network,
                        "no scripted response",
                    ))
                });
            }
            queue.remove(0)
        };
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(response) })
    }
}

fn response(body: &str, status: u16, etag: Option<&str>) -> FetchResponse {
    FetchResponse {
        final_url: "https://example.com/doc".to_string(),
        status,
        content_type: "text/html; charset=utf-8".to_string(),
        body: body.as_bytes().to_vec(),
        etag: etag.map(str::to_string),
        last_modified: None,
    }
}

fn article(paragraphs: usize) -> String {
    let mut body = String::from("<html><body><article><h1>Guide</h1>");
    body.push_str("<p>Installation: run the installer and accept the defaults.</p>");
    for index in 0..paragraphs {
        body.push_str(&format!(
            "<p>Filler paragraph number {index} that talks about unrelated things at length.</p>"
        ));
    }
    body.push_str("</article></body></html>");
    body
}

fn tool(
    responses: Vec<FetchResponse>,
) -> (FetchTool, Arc<Fake>, Arc<ResponseStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(responses));
    let store = Arc::new(ResponseStore::new(Duration::from_secs(300), 8 * 1024 * 1024));
    let tool = FetchTool::new(fake.clone(), dir.path(), store.clone());
    (tool, fake, store, dir)
}

#[tokio::test]
async fn the_second_read_is_served_from_the_cache() {
    let (tool, fake, _store, _dir) = tool(vec![response(&article(3), 200, Some("\"v1\""))]);

    let first = tool.handle_fetch(json!({"url": "https://example.com/doc"})).await;
    assert!(
        first.content[0].text.contains("source: network"),
        "{}",
        first.content[0].text
    );

    let second = tool.handle_fetch(json!({"url": "https://example.com/doc"})).await;
    assert!(
        second.content[0].text.contains("source: cache"),
        "{}",
        second.content[0].text
    );
    assert_eq!(fake.calls(), 1, "the cache must not touch the network again");
}

#[tokio::test]
async fn revalidation_returns_the_page_without_repaying_for_it() {
    let (tool, fake, _store, _dir) = tool(vec![
        response(&article(3), 200, Some("\"v1\"")),
        response("", 304, Some("\"v1\"")),
    ]);

    tool.handle_fetch(json!({"url": "https://example.com/doc"}))
        .await;
    let revalidated = tool
        .handle_fetch(json!({"url": "https://example.com/doc", "refresh": true}))
        .await;

    assert!(
        revalidated.content[0].text.contains("source: revalidated"),
        "{}",
        revalidated.content[0].text
    );
    assert!(revalidated.content[0].text.contains("Installation"));
    assert_eq!(fake.calls(), 2);
}

#[tokio::test]
async fn a_small_budget_puts_the_overflow_in_a_handle() {
    let (tool, _fake, store, _dir) = tool(vec![response(&article(40), 200, None)]);

    let result = tool
        .handle_fetch(json!({"url": "https://example.com/doc", "budget_tokens": 120}))
        .await;
    let text = &result.content[0].text;

    assert!(text.contains("Resposta truncada"), "{text}");
    let handle = text
        .split("handle: `")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .expect("handle in envelope");
    assert!(store.get(handle).is_some(), "the overflow must be peekable");
    assert!(
        !text.contains("Filler paragraph number 39"),
        "only the budget should be sent"
    );
}

#[tokio::test]
async fn the_question_decides_which_block_comes_first() {
    let (tool, _fake, _store, _dir) = tool(vec![response(&article(6), 200, None)]);

    let result = tool
        .handle_fetch(json!({
            "url": "https://example.com/doc",
            "question": "installation installer",
            "budget_tokens": 4000
        }))
        .await;
    let text = &result.content[0].text;

    let installation = text
        .find("Installation: run the installer")
        .expect("install block");
    let filler = text.find("Filler paragraph number 0").expect("filler block");
    assert!(
        installation < filler,
        "the relevant block must lead: {text}"
    );
}

#[tokio::test]
async fn a_shell_without_text_is_reported_as_js_only() {
    let (tool, _fake, _store, _dir) = tool(vec![response(
        "<html><body><div id=\"root\"></div><script>boot()</script></body></html>",
        200,
        None,
    )]);

    let result = tool
        .handle_fetch(json!({"url": "https://example.com/app"}))
        .await;
    let text = &result.content[0].text;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(text.contains("js-only"), "{text}");
    assert!(text.contains("dica:"), "{text}");
}
