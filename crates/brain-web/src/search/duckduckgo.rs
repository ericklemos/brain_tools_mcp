use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use scraper::{Html, Node, Selector};

use super::{Hit, SearchProvider, SearchRequest};
use crate::error::{FailureKind, Result, WebError};
use crate::http::{FetchRequest, Fetcher};

/// The HTML endpoint is the free one: no key, no JS, and links come wrapped in
/// a redirect that carries the real URL in `uddg`.
pub struct DuckDuckGo {
    fetcher: Arc<dyn Fetcher>,
}

impl DuckDuckGo {
    pub fn new(fetcher: Arc<dyn Fetcher>) -> Self {
        Self { fetcher }
    }
}

impl SearchProvider for DuckDuckGo {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn search(
        &self,
        request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>>> + Send + '_>> {
        let query = request.query.clone();
        let max_results = request.max_results;
        let locale = request.locale.clone();
        Box::pin(async move {
            let region = region_for(locale.as_deref());
            let url = format!(
                "https://html.duckduckgo.com/html/?q={}&kl={region}",
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

            if matches!(response.status, 202 | 429) {
                return Err(WebError::new(
                    FailureKind::RateLimited,
                    "duckduckgo asked the client to slow down",
                ));
            }
            if !(200..300).contains(&response.status) {
                return Err(WebError::new(
                    FailureKind::Network,
                    format!("duckduckgo returned HTTP {}", response.status),
                ));
            }

            let html = String::from_utf8_lossy(&response.body);
            Ok(parse(&html, max_results))
        })
    }
}

/// Parses both the `html.duckduckgo.com` layout (div.result) and the
/// lite layout (table rows), because the endpoint can serve either.
pub fn parse(html: &str, limit: usize) -> Vec<Hit> {
    let document = Html::parse_document(html);
    let block = Selector::parse("div.result, div.web-result").expect("static selector");
    let link = Selector::parse("a.result__a").expect("static selector");
    let snippet = Selector::parse(".result__snippet").expect("static selector");

    let mut hits: Vec<Hit> = Vec::new();
    for node in document.select(&block) {
        let Some(anchor) = node.select(&link).next() else {
            continue;
        };
        let Some(url) = real_url(anchor.value().attr("href").unwrap_or_default()) else {
            continue;
        };
        let title = normalize(&anchor.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let text = node
            .select(&snippet)
            .next()
            .map(|cell| normalize(&cell.text().collect::<String>()))
            .unwrap_or_default();
        hits.push(Hit {
            title,
            url,
            snippet: text,
            provider: "duckduckgo",
            rank: hits.len(),
        });
        if hits.len() >= limit {
            return hits;
        }
    }

    if !hits.is_empty() {
        return hits;
    }

    // Lite layout: the snippet is a sibling row, so walk in document order and
    // attach it to the link that came just before.
    for node in document.tree.root().descendants() {
        let Node::Element(element) = node.value() else {
            continue;
        };
        let class = element.attr("class").unwrap_or_default();
        if element.name() == "a" && class.split_whitespace().any(|name| name == "result-link") {
            let Some(anchor) = scraper::ElementRef::wrap(node) else {
                continue;
            };
            let Some(url) = real_url(anchor.value().attr("href").unwrap_or_default()) else {
                continue;
            };
            let title = normalize(&anchor.text().collect::<String>());
            if title.is_empty() {
                continue;
            }
            hits.push(Hit {
                title,
                url,
                snippet: String::new(),
                provider: "duckduckgo",
                rank: hits.len(),
            });
        } else if element.name() == "td"
            && class.split_whitespace().any(|name| name == "result-snippet")
        {
            if let Some(last) = hits.last_mut() {
                if last.snippet.is_empty() {
                    if let Some(cell) = scraper::ElementRef::wrap(node) {
                        last.snippet = normalize(&cell.text().collect::<String>());
                    }
                }
            }
        }
        if hits.len() >= limit {
            break;
        }
    }

    hits
}

fn real_url(href: &str) -> Option<String> {
    if let Some(rest) = href.split("uddg=").nth(1) {
        let value = rest.split('&').next()?;
        let decoded = percent_decode(value);
        if decoded.starts_with("http") {
            return Some(decoded);
        }
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    None
}

fn region_for(locale: Option<&str>) -> &'static str {
    match locale.map(|tag| tag.to_ascii_lowercase()) {
        Some(tag) if tag.starts_with("pt") => "br-pt",
        Some(tag) if tag.starts_with("es") => "es-es",
        Some(tag) if tag.starts_with("de") => "de-de",
        Some(tag) if tag.starts_with("fr") => "fr-fr",
        _ => "us-en",
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

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERP: &str = r##"
    <html><body>
      <div class="result results_links">
        <h2 class="result__title">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&amp;rut=abc">The Rust Book</a>
        </h2>
        <a class="result__snippet" href="#">Learn Rust with the official book.</a>
      </div>
      <div class="result results_links">
        <h2 class="result__title"><a class="result__a" href="https://example.com/rust">Example Rust</a></h2>
        <a class="result__snippet">A different page about the language.</a>
      </div>
    </body></html>
    "##;

    #[test]
    fn redirect_links_are_unwrapped() {
        let hits = parse(SERP, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://doc.rust-lang.org/book/");
        assert_eq!(hits[0].title, "The Rust Book");
        assert_eq!(hits[0].snippet, "Learn Rust with the official book.");
        assert_eq!(hits[1].url, "https://example.com/rust");
    }

    #[test]
    fn the_limit_is_respected() {
        let hits = parse(SERP, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn the_lite_layout_is_understood() {
        let lite = r#"
        <html><body><table>
          <tr><td><a class="result-link" href="https://example.org/a">Lite result</a></td></tr>
          <tr><td class="result-snippet">Snippet for the lite result.</td></tr>
        </table></body></html>
        "#;
        let hits = parse(lite, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.org/a");
        assert_eq!(hits[0].snippet, "Snippet for the lite result.");
    }

    #[test]
    fn the_query_is_percent_encoded() {
        assert_eq!(encode("rust async io"), "rust+async+io");
        assert_eq!(encode("café"), "caf%C3%A9");
    }
}
