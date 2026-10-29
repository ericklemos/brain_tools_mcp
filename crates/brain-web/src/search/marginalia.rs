use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use scraper::{Html, Selector};

use super::{Hit, SearchProvider, SearchRequest};
use crate::error::{FailureKind, Result, WebError};
use crate::http::{FetchRequest, Fetcher};

/// Marginalia is an independent index with a plain HTML endpoint. It is small,
/// but it answers when the big frontends decide to show a captcha.
pub struct Marginalia {
    fetcher: Arc<dyn Fetcher>,
}

impl Marginalia {
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self { fetcher }
    }
}

impl SearchProvider for Marginalia {
    fn name(&self) -> &'static str {
        "marginalia"
    }

    fn search(
        &self,
        request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>>> + Send + '_>> {
        let query = request.query.clone();
        let max_results = request.max_results;
        Box::pin(async move {
            let url = format!(
                "https://old-search.marginalia.nu/search?query={}",
                encode(&query)
            );
            let response = self
                .fetcher
                .fetch(FetchRequest {
                    url,
                    etag: None,
                    last_modified: None,
                    accept: Some("text/html".to_string()),
                })
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(WebError::new(
                    FailureKind::Network,
                    format!("marginalia returned HTTP {}", response.status),
                ));
            }
            let html = String::from_utf8_lossy(&response.body);
            Ok(parse(&html, max_results))
        })
    }
}

pub fn parse(html: &str, limit: usize) -> Vec<Hit> {
    let document = Html::parse_document(html);
    let block = Selector::parse("section.search-result").expect("static selector");
    let title = Selector::parse("a.title").expect("static selector");
    let description = Selector::parse("p.description").expect("static selector");
    let url_cell = Selector::parse("div.url a").expect("static selector");

    let mut hits = Vec::new();
    for node in document.select(&block) {
        let Some(anchor) = node.select(&title).next() else {
            continue;
        };
        let url = anchor
            .value()
            .attr("href")
            .filter(|href| href.starts_with("http"))
            .map(str::to_string)
            .or_else(|| {
                node.select(&url_cell)
                    .next()
                    .and_then(|cell| cell.value().attr("href"))
                    .filter(|href| href.starts_with("http"))
                    .map(str::to_string)
            });
        let Some(url) = url else {
            continue;
        };
        let title = normalize(&anchor.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = node
            .select(&description)
            .next()
            .map(|cell| normalize(&cell.text().collect::<String>()))
            .unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet,
            provider: "marginalia",
            rank: hits.len(),
        });
        if hits.len() >= limit {
            break;
        }
    }
    hits
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

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_sections_are_understood() {
        let html = r#"
        <html><body>
        <section data-ms-rank="1" class="card search-result">
          <div class="url"><a href="https://example.org/page">https://example.org/page</a></div>
          <h2><a class="title" href="https://example.org/page">A useful page</a></h2>
          <p class="description">Short description of the page.</p>
        </section>
        </body></html>
        "#;
        let hits = parse(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.org/page");
        assert_eq!(hits[0].title, "A useful page");
        assert_eq!(hits[0].snippet, "Short description of the page.");
    }
}
