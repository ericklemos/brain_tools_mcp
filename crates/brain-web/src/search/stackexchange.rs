use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::{Hit, SearchProvider, SearchRequest};
use crate::error::{FailureKind, Result, WebError};
use crate::http::{FetchRequest, Fetcher};

/// Stack Exchange has a free JSON API and is where most coding questions
/// actually land. No key means a small quota, which is enough for a session.
pub struct StackExchange {
    fetcher: Arc<dyn Fetcher>,
}

impl StackExchange {
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self { fetcher }
    }
}

impl SearchProvider for StackExchange {
    fn name(&self) -> &'static str {
        "stackexchange"
    }

    fn search(
        &self,
        request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>>> + Send + '_>> {
        let query = request.query.clone();
        let max_results = request.max_results;
        Box::pin(async move {
            let url = format!(
                "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&q={}&site=stackoverflow&pagesize={}&filter=default",
                encode(&query),
                max_results.min(20)
            );
            let response = self
                .fetcher
                .fetch(FetchRequest {
                    url,
                    etag: None,
                    last_modified: None,
                    accept: Some("application/json".to_string()),
                })
                .await?;
            if response.status == 429 {
                return Err(WebError::new(
                    FailureKind::RateLimited,
                    "stackexchange quota exhausted",
                ));
            }
            if !(200..300).contains(&response.status) {
                return Err(WebError::new(
                    FailureKind::Network,
                    format!("stackexchange returned HTTP {}", response.status),
                ));
            }
            parse(&response.body, max_results)
        })
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    title: String,
    link: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    answer_count: i64,
    #[serde(default)]
    tags: Vec<String>,
}

pub fn parse(body: &[u8], limit: usize) -> Result<Vec<Hit>> {
    let response: Response = serde_json::from_slice(body)
        .map_err(|error| WebError::new(FailureKind::Unsupported, format!("stackexchange JSON: {error}")))?;

    Ok(response
        .items
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(rank, item)| {
            let snippet = format!(
                "score {} · {} respostas · tags: {}",
                item.score,
                item.answer_count,
                item.tags.join(", ")
            );
            Hit {
                title: unescape(&item.title),
                url: item.link,
                snippet,
                provider: "stackexchange",
                rank,
            }
        })
        .collect())
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_become_hits_with_a_useful_snippet() {
        let body = br#"{"items":[{"title":"How to &quot;await&quot; a future?","link":"https://stackoverflow.com/q/1","score":42,"answer_count":3,"tags":["rust","async"]}]}"#;
        let hits = parse(body, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "How to \"await\" a future?");
        assert_eq!(hits[0].url, "https://stackoverflow.com/q/1");
        assert!(hits[0].snippet.contains("score 42"));
        assert!(hits[0].snippet.contains("rust, async"));
    }
}
