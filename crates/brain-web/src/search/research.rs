//! `brain_research`: search, read the best sources, return the evidence.
//!
//! The alternative is three round trips (search, pick, fetch). Here they are
//! one call: the providers are fused, the top sources are fetched in parallel,
//! and only the blocks that answer the question survive the budget. Every
//! excerpt keeps its URL and anchor, so the answer is citable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_kernel::budget::{self, Section};
use brain_kernel::plugin::ToolPlugin;
use brain_kernel::protocol::{CallToolResult, Tool};
use brain_kernel::store::ResponseStore;
use serde::Deserialize;

use super::{fuse, host_of, SearchProvider, SearchRequest};
use crate::error::{FailureKind, Result, WebError};
use crate::fetch_tool::{Evidence, FetchTool};
use crate::rank;

/// Where the research tool gets a page. A trait so the tool can be tested
/// without a socket, and so a future renderer can plug in behind it.
pub trait EvidenceSource: Send + Sync {
    fn evidence(
        &self,
        url: String,
        question: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Evidence>> + Send + '_>>;
}

impl EvidenceSource for FetchTool {
    fn evidence(
        &self,
        url: String,
        question: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<Evidence>> + Send + '_>> {
        Box::pin(async move { FetchTool::evidence(self, &url, question.as_deref()).await })
    }
}

#[derive(Debug, Deserialize)]
pub struct ResearchParams {
    pub query: String,
    /// Defaults to the query. The blocks are ranked against it.
    pub question: Option<String>,
    pub max_sources: Option<usize>,
    pub budget_tokens: Option<usize>,
    pub locale: Option<String>,
}

pub struct ResearchTool {
    providers: Vec<Arc<dyn SearchProvider>>,
    source: Arc<dyn EvidenceSource>,
    store: Arc<ResponseStore>,
    default_budget_tokens: usize,
    default_max_sources: usize,
    blocks_per_source: usize,
}

impl ResearchTool {
    pub fn new(
        providers: Vec<Arc<dyn SearchProvider>>,
        source: Arc<dyn EvidenceSource>,
        store: Arc<ResponseStore>,
    ) -> Self {
        Self {
            providers,
            source,
            store,
            default_budget_tokens: 1200,
            default_max_sources: 3,
            blocks_per_source: 2,
        }
    }

    pub fn with_budget(mut self, default_budget_tokens: usize) -> Self {
        self.default_budget_tokens = default_budget_tokens;
        self
    }

    pub async fn handle_research(&self, arguments: serde_json::Value) -> CallToolResult {
        let params: ResearchParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(error) => return CallToolResult::error(format!("Invalid arguments: {error}")),
        };
        if params.query.trim().is_empty() {
            return CallToolResult::error("brain_research precisa de uma query não vazia.");
        }

        let max_sources = params.max_sources.unwrap_or(self.default_max_sources).clamp(1, 6);
        let question = params
            .question
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| params.query.clone());
        let request = SearchRequest {
            query: params.query.trim().to_string(),
            max_results: max_sources * 2,
            locale: params.locale.clone(),
        };

        let mut health: Vec<String> = Vec::new();
        let mut per_provider: Vec<(&'static str, Vec<super::Hit>)> = Vec::new();
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
            return CallToolResult::error(WebError::new(
                FailureKind::Unsupported,
                format!(
                    "brain_research: nenhum provedor devolveu resultados para {:?} ({})",
                    params.query,
                    health.join(", ")
                ),
            )
            .render());
        }

        let hits = fuse(per_provider, max_sources);
        let sources_found = hits.len();

        let mut set = tokio::task::JoinSet::new();
        for (index, hit) in hits.into_iter().enumerate() {
            let source = self.source.clone();
            let question = question.clone();
            let url = hit.url.clone();
            let host = host_of(&hit.url);
            let title = hit.title.clone();
            set.spawn(async move {
                let evidence = source.evidence(url.clone(), Some(question)).await;
                (index, title, url, host, evidence)
            });
        }

        let mut collected = Vec::new();
        let mut failures: Vec<(usize, String, String, &'static str)> = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((index, title, url, host, Ok(evidence))) => {
                    collected.push((index, title, url, host, evidence));
                }
                Ok((index, _, url, _, Err(error))) => {
                    failures.push((index, url, String::new(), error.kind.as_str()));
                }
                Err(_) => failures.push((usize::MAX, String::new(), String::new(), "join")),
            }
        }
        collected.sort_by_key(|(index, ..)| *index);
        failures.sort_by_key(|(index, ..)| *index);

        CallToolResult::text(self.render(
            &params,
            question,
            health,
            sources_found,
            collected,
            failures,
        ))
    }

    fn render(
        &self,
        params: &ResearchParams,
        question: String,
        health: Vec<String>,
        sources_found: usize,
        collected: Vec<(usize, String, String, String, Evidence)>,
        failures: Vec<(usize, String, String, &'static str)>,
    ) -> String {
        let budget_tokens = params.budget_tokens.unwrap_or(self.default_budget_tokens);
        let query = params.query.trim();

        let mut header = format!(
            "> question: {question}\n> sources: {}/{} · budget: {budget_tokens} tokens\n> providers: {}",
            collected.len(),
            sources_found,
            health.join(", ")
        );
        for (_, url, _, kind) in &failures {
            if !url.is_empty() {
                header.push_str(&format!("\n> falhou: {url} ({kind})"));
            }
        }

        let mut sections = vec![Section {
            heading: format!("### brain_research · \"{query}\""),
            body: header,
            priority: 255,
        }];

        for (position, (_, title, url, host, evidence)) in collected.iter().enumerate() {
            sections.push(Section {
                heading: format!("#### {}. {} — {host}", position + 1, title.trim()),
                body: format!(
                    "> {url} · source: {} · {} blocos · hash: {}",
                    evidence.source,
                    evidence.total_blocks,
                    &evidence.content_hash[..evidence.content_hash.len().min(10)]
                ),
                priority: 250u8.saturating_sub(position.min(6) as u8),
            });

            let order = rank::rank_blocks(&evidence.blocks, &question);
            for (slot, index) in order.into_iter().take(self.blocks_per_source).enumerate() {
                let block = &evidence.blocks[index];
                let label = if block.heading.is_empty() {
                    block.kind.as_str().to_string()
                } else {
                    block.heading.clone()
                };
                sections.push(Section {
                    heading: format!("[{}] {label}", block.anchor()),
                    body: render_block(block),
                    priority: 240u8
                        .saturating_sub((position * 4 + slot).min(200) as u8),
                });
            }
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

fn render_block(block: &crate::model::Block) -> String {
    if block.kind == crate::model::BlockKind::Code && !block.text.starts_with("```") {
        format!("```\n{}\n```", block.text)
    } else {
        block.text.clone()
    }
}

impl ToolPlugin for ResearchTool {
    fn name(&self) -> &'static str {
        "brain_research"
    }

    fn schema(&self) -> Tool {
        brain_research_tool()
    }

    fn call(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = CallToolResult> + Send + '_>> {
        Box::pin(self.handle_research(args))
    }
}

pub fn brain_research_tool() -> Tool {
    Tool {
        name: "brain_research".to_string(),
        description: "Busca, lê as melhores fontes e devolve a evidência numa chamada. Funde provedores gratuitos, baixa os top resultados em paralelo e entrega só os blocos que respondem à pergunta, com URL e âncora para citar. Use quando a pergunta precisa de fontes externas; para uma URL já conhecida, use brain_fetch.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Consulta de busca."
                },
                "question": {
                    "type": "string",
                    "description": "O que você quer saber. Ranqueia os blocos de cada fonte; sem ela, a própria query é usada."
                },
                "max_sources": {
                    "type": "integer",
                    "default": 3,
                    "minimum": 1,
                    "maximum": 6,
                    "description": "Quantas fontes baixar e ler."
                },
                "budget_tokens": {
                    "type": "integer",
                    "description": "Teto de tokens (excedente em brain_peek)."
                },
                "locale": {
                    "type": "string",
                    "description": "Região preferida, ex: pt-BR ou en-US."
                }
            },
            "required": ["query"]
        }),
    }
}
