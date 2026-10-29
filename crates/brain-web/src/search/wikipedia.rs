use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::{Hit, SearchProvider, SearchRequest};
use crate::error::{FailureKind, Result, WebError};
use crate::http::{FetchRequest, Fetcher};

/// Wikipedia answers "what is X" without a captcha and with a stable API.
pub struct Wikipedia {
    fetcher: Arc<dyn Fetcher>,
}

impl Wikipedia {
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self { fetcher }
    }
}

impl SearchProvider for Wikipedia {
    fn name(&self) -> &'static str {
        "wikipedia"
    }

    fn search(
        &self,
        request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>>> + Send + '_>> {
        let query = request.query.clone();
        let max_results = request.max_results;
        let lang = language(request.locale.as_deref());
        Box::pin(async move {
            let url = format!(
                "https://{lang}.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit={}",
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
            if !(200..300).contains(&response.status) {
                return Err(WebError::new(
                    FailureKind::Network,
                    format!("wikipedia returned HTTP {}", response.status),
                ));
            }
            parse(&response.body, &lang, max_results)
        })
    }
}

#[derive(Deserialize)]
struct Response {
    query: Option<Query>,
}

#[derive(Deserialize)]
struct Query {
    #[serde(default)]
    search: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    title: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    pageid: i64,
}

pub fn parse(body: &[u8], lang: &str, limit: usize) -> Result<Vec<Hit>> {
    let response: Response = serde_json::from_slice(body)
        .map_err(|error| WebError::new(FailureKind::Unsupported, format!("wikipedia JSON: {error}")))?;
    let Some(query) = response.query else {
        return Ok(Vec::new());
    };

    Ok(query
        .search
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(rank, item)| Hit {
            title: item.title,
            url: format!("https://{lang}.wikipedia.org/?curid={}", item.pageid),
            snippet: strip_tags(&item.snippet),
            provider: "wikipedia",
            rank,
        })
        .collect())
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn language(locale: Option<&str>) -> String {
    match locale {
        Some(tag) if tag.len() >= 2 => tag[..2].to_ascii_lowercase(),
        _ => "en".to_string(),
    }
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
    fn search_hits_are_turned_into_article_links() {
        let body = br#"{"query":{"search":[{"title":"Async/await","snippet":"Kotlin, <span class=\"searchmatch\">Rust</span>, Nim","pageid":40844675}]}}"#;
        let hits = parse(body, "en", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://en.wikipedia.org/?curid=40844675");
        assert_eq!(hits[0].snippet, "Kotlin, Rust, Nim");
    }

    #[test]
    fn the_locale_picks_the_wiki() {
        assert_eq!(language(Some("pt-BR")), "pt");
        assert_eq!(language(None), "en");
    }
}
