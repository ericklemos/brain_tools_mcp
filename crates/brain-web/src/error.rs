use std::fmt;

/// Why a fetch failed.
///
/// The kind is part of the contract, not decoration: an agent can act on
/// `rate-limited` (wait), `paywall` (find another source), or `js-only` (stop
/// trying). Collapsing these into a message string is what makes a web tool
/// opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    InvalidUrl,
    Network,
    Timeout,
    RateLimited,
    NotFound,
    Forbidden,
    Captcha,
    Paywall,
    TooLarge,
    Binary,
    Unsupported,
    JsOnly,
    Cache,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid-url",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate-limited",
            Self::NotFound => "not-found",
            Self::Forbidden => "forbidden",
            Self::Captcha => "captcha",
            Self::Paywall => "paywall",
            Self::TooLarge => "too-large",
            Self::Binary => "binary",
            Self::Unsupported => "unsupported",
            Self::JsOnly => "js-only",
            Self::Cache => "cache",
        }
    }

    /// What the caller should do next. This is the part a retry loop cannot
    /// infer from an HTTP status.
    pub fn hint(self) -> &'static str {
        match self {
            Self::InvalidUrl => "pass an absolute http(s) URL",
            Self::Network => "retry once; if it persists the host is unreachable",
            Self::Timeout => "retry with a larger timeout or a smaller page",
            Self::RateLimited => "wait before retrying, or use another source",
            Self::NotFound => "the URL does not exist; do not retry",
            Self::Forbidden => "the host refuses this client; do not retry blindly",
            Self::Captcha => "bot protection; a static fetch will not pass, use another source",
            Self::Paywall => "content is behind a paywall; find another source",
            Self::TooLarge => "raise the byte limit or fetch a narrower URL",
            Self::Binary => "not text; this tool only reads text documents",
            Self::Unsupported => "no readable blocks in this document",
            Self::JsOnly => "the content is rendered by JavaScript; needs a browser renderer",
            Self::Cache => "the local cache could not be read or written",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebError {
    pub kind: FailureKind,
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

impl WebError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// The one-line contract the tool returns on failure.
    pub fn render(&self) -> String {
        let mut out = format!(
            "brain_fetch falhou [{}]: {}\n\ndica: {}",
            self.kind.as_str(),
            self.message,
            self.kind.hint()
        );
        out.push_str(&format!("\nkind: {}", self.kind.as_str()));
        if let Some(secs) = self.retry_after_secs {
            out.push_str(&format!("\nretry-after: {secs}s"));
        }
        out
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for WebError {}

pub type Result<T> = std::result::Result<T, WebError>;
