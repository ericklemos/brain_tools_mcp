use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;

use crate::cache::{self, CachedPage, WebCache};
use crate::error::{FailureKind, Result, WebError};
use crate::extract;
use crate::http::{FetchRequest, FetchResponse, Fetcher};
use crate::model::{Block, BlockKind, Page};
use crate::rank;
use crate::util::{iso8601, now_secs};

#[derive(Debug, Deserialize)]
pub struct FetchParams {
    pub url: String,
    /// What the caller wants to know. Blocks are ranked against it; without it
    /// the document order is the answer.
    pub question: Option<String>,
    pub budget_tokens: Option<usize>,
    pub max_blocks: Option<usize>,
    /// Bypass the local cache and revalidate with the server.
    pub refresh: Option<bool>,
}

pub struct FetchTool {
    fetcher: Arc<dyn Fetcher>,
    cache: WebCache,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
    default_max_blocks: usize,
    ttl: Duration,
}

impl FetchTool {
    pub fn new(
        fetcher: Arc<dyn Fetcher>,
        root_dir: impl Into<std::path::PathBuf>,
        store: Arc<ResponseStore>,
    ) -> Self {
        Self {
            fetcher,
            cache: WebCache::new(root_dir),
            store,
            default_budget_tokens: 2000,
            default_max_blocks: 80,
            ttl: Duration::from_secs(900),
        }
    }

    pub fn with_budget(mut self, default_budget_tokens: usize) -> Self {
        self.default_budget_tokens = default_budget_tokens;
        self
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub async fn handle_fetch(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: FetchParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(error) => return CallToolResult::error(format!("Invalid arguments: {error}")),
        };

        match self.read(&params).await {
            Ok(read) => CallToolResult::text(self.render(&params, read)),
            Err(error) => CallToolResult::error(error.render()),
        }
    }

    async fn read(&self, params: &FetchParams) -> Result<Read> {
        let url = cache::normalize_url(&params.url);
        let cached = self.cache.load(&url);
        let refresh = params.refresh.unwrap_or(false);

        // A fresh cache entry is the whole point: the second read of the same
        // URL must not cost a request, let alone a body.
        if !refresh {
            if let Some(entry) = &cached {
                if entry.age() < self.ttl {
                    return Ok(Read::from_cache(entry, "cache"));
                }
            }
        }

        let request = FetchRequest {
            url: url.clone(),
            etag: cached.as_ref().and_then(|entry| entry.etag.clone()),
            last_modified: cached.as_ref().and_then(|entry| entry.last_modified.clone()),
        };

        match self.fetcher.fetch(request).await {
            Ok(response) if response.status == 304 => match cached {
                Some(entry) => {
                    self.cache.touch(&url)?;
                    Ok(Read::from_cache(&entry, "revalidated"))
                }
                // A 304 without a body is useless; ask again unconditionally.
                None => {
                    let response = self
                        .fetcher
                        .fetch(FetchRequest {
                            url: url.clone(),
                            etag: None,
                            last_modified: None,
                        })
                        .await?;
                    self.absorb(response, &url)
                }
            },
            Ok(response) => self.absorb(response, &url),
            // A stale copy beats an empty answer, as long as the staleness is
            // declared in the response instead of hidden.
            Err(error) => match cached {
                Some(entry) => Ok(Read {
                    note: Some(format!("network failed ({})", error.kind.as_str())),
                    ..Read::from_cache(&entry, "cache")
                }),
                None => Err(error),
            },
        }
    }

    fn absorb(&self, response: FetchResponse, url: &str) -> Result<Read> {
        let page = page_from_response(&response, url)?;
        let content_hash = blake3::hash(&response.body).to_hex().to_string();
        let entry = CachedPage {
            url: url.to_string(),
            final_url: response.final_url.clone(),
            etag: response.etag.clone(),
            last_modified: response.last_modified.clone(),
            fetched_at: now_secs(),
            content_hash: content_hash.clone(),
            page: page.clone(),
        };
        self.cache.store(&entry, &response.body)?;

        Ok(Read {
            page,
            source: "network",
            status: response.status,
            fetched_at: entry.fetched_at,
            content_hash,
            final_url: entry.final_url,
            note: None,
        })
    }

    fn render(&self, params: &FetchParams, read: Read) -> String {
        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let max_blocks = params.max_blocks.unwrap_or(self.default_max_blocks).max(1);
        let question = params.question.as_deref().filter(|text| !text.trim().is_empty());
        let total = read.page.blocks.len();

        let mut sections = vec![Section {
            heading: format!("### brain_fetch · {}", read.page.meta.title),
            body: provenance(&read, params, total, budget_tokens),
            // The header is never the thing that gets dropped: an answer
            // without provenance is not an answer.
            priority: 255,
        }];

        let order = rank::rank_blocks(&read.page.blocks, question.unwrap_or(""));
        for (position, index) in order.into_iter().take(max_blocks).enumerate() {
            let block = &read.page.blocks[index];
            let label = if block.heading.is_empty() {
                block.kind.as_str().to_string()
            } else {
                block.heading.clone()
            };
            let priority = if question.is_some() {
                240u8.saturating_sub(position.min(200) as u8)
            } else {
                240u8.saturating_sub(index.min(200) as u8)
            };
            sections.push(Section {
                heading: format!("[{}] {}", block.anchor(), label),
                body: render_block(block),
                priority,
            });
        }

        let selection = budget::select(&sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        budget::render(&selection, handle.as_deref())
    }
}

impl ToolPlugin for FetchTool {
    fn name(&self) -> &'static str {
        "brain_fetch"
    }

    fn schema(&self) -> Tool {
        brain_fetch_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_fetch(args))
    }
}

struct Read {
    page: Page,
    source: &'static str,
    status: u16,
    fetched_at: u64,
    content_hash: String,
    final_url: String,
    note: Option<String>,
}

impl Read {
    fn from_cache(entry: &CachedPage, source: &'static str) -> Self {
        Self {
            page: entry.page.clone(),
            source,
            status: 200,
            fetched_at: entry.fetched_at,
            content_hash: entry.content_hash.clone(),
            final_url: entry.final_url.clone(),
            note: None,
        }
    }
}

fn page_from_response(response: &FetchResponse, url: &str) -> Result<Page> {
    let content_type = response.content_type.to_ascii_lowercase();
    let body = String::from_utf8_lossy(&response.body);

    if content_type.contains("html") || content_type.contains("xhtml") {
        return extract::extract(&body, url);
    }

    if content_type.contains("json") {
        let pretty = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| body.to_string());
        return Ok(extract::single_block(url, BlockKind::Code, pretty));
    }

    if content_type.starts_with("text/")
        || content_type.contains("markdown")
        || content_type.contains("xml")
        || content_type.is_empty()
    {
        return Ok(extract::single_block(url, BlockKind::Paragraph, body.to_string()));
    }

    Err(WebError::new(
        FailureKind::Binary,
        format!("unsupported content type '{}'", response.content_type),
    ))
}

fn render_block(block: &Block) -> String {
    if block.kind == BlockKind::Code && !block.text.starts_with("```") {
        format!("```\n{}\n```", block.text)
    } else {
        block.text.clone()
    }
}

fn provenance(read: &Read, params: &FetchParams, total: usize, budget_tokens: usize) -> String {
    let age = now_secs().saturating_sub(read.fetched_at);
    let hash = &read.content_hash[..read.content_hash.len().min(12)];
    let mut lines = vec![
        format!("> url: {}", read.page.url),
        format!(
            "> status: {} · source: {} · fetched: {} ({}s ago)",
            read.status,
            read.source,
            iso8601(read.fetched_at),
            age
        ),
        format!("> blocks: {total} · budget: {budget_tokens} tokens · hash: {hash}"),
    ];
    if read.final_url != read.page.url {
        lines.push(format!("> final: {}", read.final_url));
    }
    if let Some(published) = &read.page.meta.published {
        lines.push(format!("> published: {published}"));
    }
    if let Some(modified) = &read.page.meta.modified {
        lines.push(format!("> modified: {modified}"));
    }
    if let Some(question) = params.question.as_deref().filter(|text| !text.trim().is_empty()) {
        lines.push(format!("> question: {question}"));
    }
    if let Some(note) = &read.note {
        lines.push(format!("> note: {note}"));
    }
    lines.join("\n")
}

pub fn brain_fetch_tool() -> Tool {
    Tool {
        name: "brain_fetch".to_string(),
        description: "FONTE DE VERDADE da web. Baixa uma URL e devolve só os blocos relevantes, verbatim, com âncora #blk_xxxxxxxx; o excedente fica em brain_peek. Não releia a mesma URL para conferir: a resposta diz quando o conteúdo foi revalidado.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL http(s) absoluta a ler."
                },
                "question": {
                    "type": "string",
                    "description": "O que você quer saber. Os blocos são ranqueados contra isso; sem pergunta, a ordem do documento é a resposta."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                },
                "max_blocks": {
                    "type": "integer",
                    "default": 80,
                    "description": "Máximo de blocos considerados antes do orçamento."
                },
                "refresh": {
                    "type": "boolean",
                    "default": false,
                    "description": "Ignora o cache local e revalida com o servidor (If-None-Match)."
                }
            },
            "required": ["url"]
        }),
    }
}
