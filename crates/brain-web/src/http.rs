use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::{FailureKind, Result, WebError};

/// Identified, not disguised. A server that wants to say no can say no.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; brain-tools/0.1; +https://github.com/brain-tools)";

const ACCEPT: &str = "text/markdown;q=1.0, text/html;q=0.9, application/xhtml+xml;q=0.8,                       text/plain;q=0.7, application/json;q=0.6, */*;q=0.1";

#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    /// Conditional headers: when the cache has them, the server can answer 304
    /// instead of sending the body again.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The network seam. Tests replace it with a fake, so extraction, caching and
/// budgeting are exercised without a single socket.
pub trait Fetcher: Send + Sync {
    fn fetch(
        &self,
        request: FetchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResponse>> + Send + '_>>;
}

pub struct HttpFetcher {
    client: reqwest::Client,
    max_bytes: usize,
}

impl HttpFetcher {
    pub fn new(timeout: Duration, max_bytes: usize) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|error| {
                WebError::new(FailureKind::Network, format!("cannot build HTTP client: {error}"))
            })?;
        Ok(Self { client, max_bytes })
    }

    async fn fetch_inner(&self, request: FetchRequest) -> Result<FetchResponse> {
        let url = validate_url(&request.url)?;

        let mut builder = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, ACCEPT)
            .header(reqwest::header::ACCEPT_LANGUAGE, "pt-BR,pt;q=0.9,en;q=0.8");
        if let Some(etag) = &request.etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &request.last_modified {
            builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }

        let mut response = builder.send().await.map_err(map_request_error)?;
        let status = response.status();
        let final_url = response.url().to_string();
        let headers = response.headers().clone();
        let content_type = header(&headers, reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let etag = header(&headers, reqwest::header::ETAG);
        let last_modified = header(&headers, reqwest::header::LAST_MODIFIED);

        if status.as_u16() == 304 {
            return Ok(FetchResponse {
                final_url,
                status: 304,
                content_type,
                body: Vec::new(),
                etag,
                last_modified,
            });
        }

        if !status.is_success() {
            return Err(status_error(status, &headers));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_request_error)? {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(WebError::new(
                    FailureKind::TooLarge,
                    format!("response exceeds the {} byte limit", self.max_bytes),
                ));
            }
            body.extend_from_slice(&chunk);
        }

        Ok(FetchResponse {
            final_url,
            status: status.as_u16(),
            content_type,
            body,
            etag,
            last_modified,
        })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(
        &self,
        request: FetchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResponse>> + Send + '_>> {
        Box::pin(self.fetch_inner(request))
    }
}

fn header(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn map_request_error(error: reqwest::Error) -> WebError {
    if error.is_timeout() {
        WebError::new(FailureKind::Timeout, format!("request timed out: {error}"))
    } else if error.is_redirect() {
        WebError::new(FailureKind::Network, format!("too many redirects: {error}"))
    } else {
        WebError::new(FailureKind::Network, error.to_string())
    }
}

fn status_error(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> WebError {
    let captured = status.as_u16();
    let message = format!("HTTP {captured} from the server");

    match captured {
        402 => WebError::new(FailureKind::Paywall, message),
        403 if headers.contains_key("cf-mitigated") => {
            WebError::new(FailureKind::Captcha, format!("{message} (Cloudflare challenge)"))
        }
        403 | 451 => WebError::new(FailureKind::Forbidden, message),
        404 | 410 => WebError::new(FailureKind::NotFound, message),
        429 => {
            let retry_after = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            WebError::new(FailureKind::RateLimited, message).with_retry_after(retry_after)
        }
        _ => WebError::new(FailureKind::Network, message),
    }
}

/// An absolute http(s) URL on a host that is not this machine or its network.
///
/// This is the lexical half of an SSRF guard: names that resolve elsewhere are
/// still allowed, because blocking DNS is the resolver's job, not ours.
pub fn validate_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|error| {
        WebError::new(FailureKind::InvalidUrl, format!("{raw}: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebError::new(
            FailureKind::InvalidUrl,
            format!("{raw}: only http and https are supported"),
        ));
    }
    let Some(host) = url.host_str() else {
        return Err(WebError::new(FailureKind::InvalidUrl, format!("{raw}: missing host")));
    };
    if is_private_host(host) {
        return Err(WebError::new(
            FailureKind::Forbidden,
            format!("{raw}: refusing to fetch a private or loopback host"),
        ));
    }
    Ok(url)
}

fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".local") || lowered.ends_with(".internal") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(address)) => address.is_loopback() || address.is_unspecified(),
        Err(_) => false,
    }
}
