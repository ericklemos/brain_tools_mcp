use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brain_kernel::store::ResponseStore;
use brain_web::search::research::{EvidenceSource, ResearchTool};
use brain_web::search::{Hit, SearchProvider, SearchRequest};
use brain_web::{Block, BlockKind, Evidence, FailureKind, WebError};
use serde_json::json;

struct FakeProvider {
    hits: Vec<Hit>,
}

impl SearchProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn search(
        &self,
        _request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>, WebError>> + Send + '_>> {
        let hits = self.hits.clone();
        Box::pin(async move { Ok(hits) })
    }
}

struct FakeSource {
    fail_on: Option<String>,
}

impl EvidenceSource for FakeSource {
    fn evidence(
        &self,
        url: String,
        _question: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Evidence, WebError>> + Send + '_>> {
        let fail_on = self.fail_on.clone();
        Box::pin(async move {
            if fail_on.as_deref() == Some(url.as_str()) {
                return Err(WebError::new(FailureKind::NotFound, "page is gone"));
            }
            Ok(Evidence {
                url: url.clone(),
                title: format!("Doc {url}"),
                source: "network",
                fetched_at: 0,
                content_hash: "abcdef1234567890".to_string(),
                total_blocks: 2,
                blocks: vec![
                    Block {
                        id: "blk_aaaaaaaaaa".to_string(),
                        kind: BlockKind::Paragraph,
                        heading: "Installation".to_string(),
                        text: "Run the installer and accept the defaults.".to_string(),
                    },
                    Block {
                        id: "blk_bbbbbbbbbb".to_string(),
                        kind: BlockKind::Paragraph,
                        heading: "Usage".to_string(),
                        text: "Call the function with a string.".to_string(),
                    },
                ],
            })
        })
    }
}

fn hit(title: &str, url: &str, rank: usize) -> Hit {
    Hit {
        title: title.to_string(),
        url: url.to_string(),
        snippet: "snippet".to_string(),
        provider: "fake",
        rank,
    }
}

fn tool(hits: Vec<Hit>, fail_on: Option<&str>) -> (ResearchTool, Arc<ResponseStore>) {
    let store = Arc::new(ResponseStore::new(Duration::from_secs(300), 8 * 1024 * 1024));
    let providers: Vec<Arc<dyn SearchProvider>> = vec![Arc::new(FakeProvider { hits })];
    let source: Arc<dyn EvidenceSource> = Arc::new(FakeSource {
        fail_on: fail_on.map(str::to_string),
    });
    (ResearchTool::new(providers, source, store.clone()), store)
}

#[tokio::test]
async fn both_sources_arrive_with_their_anchors() {
    let (research, _store) = tool(
        vec![
            hit("First", "https://a.example/one", 0),
            hit("Second", "https://b.example/two", 1),
        ],
        None,
    );

    let result = research
        .handle_research(json!({"query": "install rust", "max_sources": 2}))
        .await;
    let text = &result.content[0].text;

    assert!(text.contains("#### 1. First — a.example"), "{text}");
    assert!(text.contains("#### 2. Second — b.example"), "{text}");
    assert!(text.contains("https://a.example/one"), "{text}");
    assert!(text.contains("[#blk_aaaaaaaaaa]"), "{text}");
    assert_eq!(result.is_error, None);
}

#[tokio::test]
async fn a_source_that_fails_does_not_sink_the_others() {
    let (research, _store) = tool(
        vec![
            hit("Broken", "https://a.example/one", 0),
            hit("Healthy", "https://b.example/two", 1),
        ],
        Some("https://a.example/one"),
    );

    let result = research
        .handle_research(json!({"query": "install rust", "max_sources": 2}))
        .await;
    let text = &result.content[0].text;

    assert!(text.contains("falhou: https://a.example/one"), "{text}");
    assert!(text.contains("#### 1. Healthy — b.example"), "{text}");
}

#[tokio::test]
async fn a_small_budget_puts_the_overflow_in_a_handle() {
    let (research, store) = tool(
        vec![
            hit("First", "https://a.example/one", 0),
            hit("Second", "https://b.example/two", 1),
        ],
        None,
    );

    let result = research
        .handle_research(json!({"query": "install rust", "max_sources": 2, "budget_tokens": 80}))
        .await;
    let text = &result.content[0].text;

    assert!(text.contains("Resposta truncada"), "{text}");
    let handle = text
        .split("handle: `")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .expect("handle in envelope");
    assert!(store.get(handle).is_some(), "overflow must be peekable");
}
