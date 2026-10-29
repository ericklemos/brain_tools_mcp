//! Free web search: several providers, one ranked list.
//!
//! The point is not to beat a paid index. It is to stop depending on any
//! single source: each provider fails on its own, and Reciprocal Rank Fusion
//! turns weak consensus into a usable order. Results are priced before the
//! caller spends anything: a URL already in the fetch cache is marked, so the
//! model knows a read is free.

pub mod duckduckgo;
pub mod marginalia;
pub mod stackexchange;
pub mod wikipedia;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;

use crate::cache::WebCache;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: usize,
    /// BCP-47-ish tag used to pick the provider's region, e.g. `pt-BR`.
    pub locale: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub provider: &'static str,
    pub rank: usize,
}

/// One free source. Implementations own their parsing and their failures.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(
        &self,
        request: &SearchRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Hit>>> + Send + '_>>;
}

/// Reciprocal Rank Fusion: a hit that appears high in several weak providers
/// beats one that appears high in only one.
pub fn fuse(results: Vec<(&'static str, Vec<Hit>)>, max_results: usize) -> Vec<Hit> {
    const K: f32 = 60.0;
    let mut best: HashMap<String, (Hit, f32)> = HashMap::new();

    for (provider, hits) in results {
        for mut hit in hits {
            hit.provider = provider;
            let key = canonical_url(&hit.url);
            let score = 1.0 / (K + (hit.rank + 1) as f32);
            match best.get_mut(&key) {
                Some((existing, total)) => {
                    *total += score;
                    // Keep the richest snippet: providers truncate differently.
                    if hit.snippet.chars().count() > existing.snippet.chars().count() {
                        existing.snippet = hit.snippet;
                    }
                    if existing.title.is_empty() {
                        existing.title = hit.title;
                    }
                }
                None => {
                    best.insert(key, (hit, score));
                }
            }
        }
    }

    let mut ranked: Vec<(Hit, f32)> = best.into_values().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.url.cmp(&right.0.url))
    });

    // Syndicated copies share a title and a host: one is enough.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    ranked
        .into_iter()
        .map(|(hit, _)| hit)
        .filter(|hit| seen.insert(dedupe_key(hit)))
        .take(max_results)
        .collect()
}

fn dedupe_key(hit: &Hit) -> String {
    let host = host_of(&hit.url);
    let title: String = hit
        .title
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect();
    format!("{host}|{title}")
}

/// Fragments and campaign parameters do not change the document, so they must
/// not create two entries.
pub fn canonical_url(url: &str) -> String {
    let mut url = url.trim().to_string();
    if let Some((head, _)) = url.split_once('#') {
        url = head.to_string();
    }

    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base.to_string(), Some(query.to_string())),
        None => (url, None),
    };

    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.split_once('/') {
                Some((host, path)) => (host, format!("/{path}")),
                None => (rest, String::new()),
            };
            let host = host.trim_start_matches("www.").to_lowercase();
            format!("{scheme}://{host}{}", path.trim_end_matches('/'))
        }
        None => base.trim_end_matches('/').to_string(),
    };

    match query {
        Some(query) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let key = pair.split('=').next().unwrap_or("");
                    !key.starts_with("utm_")
                        && !matches!(key, "fbclid" | "gclid" | "mc_cid" | "mc_eid" | "ref")
                })
                .collect();
            if kept.is_empty() {
                base
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
        None => base,
    }
}

pub fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or("").to_string())
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_string()
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub query: String,
    pub max_results: Option<usize>,
    pub locale: Option<String>,
    pub budget_tokens: Option<usize>,
}

pub struct WebSearchTool {
    providers: Vec<Arc<dyn SearchProvider>>,
    cache: WebCache,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
    default_max_results: usize,
}

impl WebSearchTool {
    pub fn new(
        providers: Vec<Arc<dyn SearchProvider>>,
        root_dir: impl Into<std::path::PathBuf>,
        store: Arc<ResponseStore>,
    ) -> Self {
        Self {
            providers,
            cache: WebCache::new(root_dir),
            store,
            default_budget_tokens: 900,
            default_max_results: 8,
        }
    }

    pub fn with_budget(mut self, default_budget_tokens: usize) -> Self {
        self.default_budget_tokens = default_budget_tokens;
        self
    }

    pub async fn handle_search(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: SearchParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(error) => return CallToolResult::error(format!("Invalid arguments: {error}")),
        };
        if params.query.trim().is_empty() {
            return CallToolResult::error("brain_websearch precisa de uma query não vazia.");
        }

        let max_results = params.max_results.unwrap_or(self.default_max_results).clamp(1, 20);
        let request = SearchRequest {
            query: params.query.trim().to_string(),
            // Ask each provider for more than we will show: fusion needs overlap
            // to discover that two sources agree.
            max_results: max_results * 2,
            locale: params.locale.clone(),
        };

        let mut per_provider: Vec<(&'static str, Vec<Hit>)> = Vec::new();
        let mut health: Vec<String> = Vec::new();
        for provider in &self.providers {
            match provider.search(&request).await {
                Ok(hits) => {
                    health.push(format!("{}=ok({})", provider.name(), hits.len()));
                    per_provider.push((provider.name(), hits));
                }
                Err(error) => health.push(format!(
                    "{}=falhou({})",
                    provider.name(),
                    error.kind.as_str()
                )),
            }
        }

        if per_provider.iter().all(|(_, hits)| hits.is_empty()) {
            return CallToolResult::error(format!(
                "brain_websearch: nenhum provedor devolveu resultados para {:?}\n> providers: {}\n> dica: reformule a query, ou use brain_fetch direto numa URL",
                params.query,
                health.join(", ")
            ));
        }

        let hits = fuse(per_provider, max_results);
        CallToolResult::text(self.render(&params, request.query, hits, health, max_results))
    }

    fn render(
        &self,
        params: &SearchParams,
        query: String,
        hits: Vec<Hit>,
        health: Vec<String>,
        max_results: usize,
    ) -> String {
        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let mut sections = vec![Section {
            heading: format!("### brain_websearch · \"{query}\""),
            body: format!(
                "> providers: {} · results: {} · budget: {budget_tokens} tokens\n> use brain_fetch com uma pergunta para ler só os blocos que importam",
                health.join(", "),
                hits.len()
            ),
            priority: 255,
        }];

        for (index, hit) in hits.iter().enumerate() {
            let cached = self
                .cache
                .load(&hit.url)
                .map(|entry| entry.page.blocks.len());
            let marker = match cached {
                Some(blocks) => format!(" · cached({blocks} blocos)"),
                None => String::new(),
            };
            let body = format!(
                "{} · {}{}\n> {}",
                hit.url,
                host_of(&hit.url),
                marker,
                hit.snippet.trim()
            );
            sections.push(Section {
                heading: format!("{}. {}", index + 1, hit.title.trim()),
                body,
                // The top result is the one least likely to be dropped.
                priority: 240u8.saturating_sub(index.min(200) as u8),
            });
        }

        let selection = budget::select(&sections, budget_tokens);
        let handle = if selection.omitted.is_empty() {
            None
        } else {
            Some(self.store.insert(selection.omitted_body()))
        };
        let body = budget::render(&selection, handle.as_deref());
        let _ = max_results;
        body
    }
}

impl ToolPlugin for WebSearchTool {
    fn name(&self) -> &'static str {
        "brain_websearch"
    }

    fn schema(&self) -> Tool {
        brain_websearch_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_search(args))
    }
}

pub fn brain_websearch_tool() -> Tool {
    Tool {
        name: "brain_websearch".to_string(),
        description: "Busca na web sem provedor pago: consulta fontes gratuitas, funde os rankings (RRF) e remove duplicatas. Devolve só título, URL, domínio e trecho, com marcação de páginas já em cache; use brain_fetch com `question` para ler os blocos relevantes.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Consulta em linguagem natural."
                },
                "max_results": {
                    "type": "integer",
                    "default": 8,
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Resultados após fusão e dedupe."
                },
                "locale": {
                    "type": "string",
                    "description": "Região preferida, ex: pt-BR ou en-US."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                }
            },
            "required": ["query"]
        }),
    }
}
