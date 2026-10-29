use scraper::{ElementRef, Html, Node};

use crate::error::{FailureKind, Result, WebError};
use crate::model::{Block, BlockKind, Page, PageMeta};

/// Anything below `body` that carries prose. `nav`, `header`, `footer` and
/// `aside` are deliberately absent: their text is chrome, and readability has
/// usually removed them already.
const BLOCK_TAGS: &[&str] = &[
    "h1", "h2", "h3", "h4", "h5", "h6", "p", "pre", "blockquote", "ul", "ol", "table", "dl",
];

/// Subtrees whose text is never evidence, even if a `p` lives inside them.
const CHROME_TAGS: &[&str] = &[
    "nav", "header", "footer", "aside", "form", "script", "style", "noscript", "svg", "button",
    "iframe", "template",
];

const MAX_BLOCKS: usize = 4_000;

/// Readable content first, then blocks.
///
/// `dom_smoothie` decides *what* is content; `scraper` decides *how* it is cut.
/// Doing the second step on the extracted article (not on the raw document) is
/// what keeps a documentation sidebar out of the token budget.
pub fn extract(html: &str, url: &str) -> Result<Page> {
    let (extracted, mut meta) = readable(html, url);
    // Readability needs enough prose to score. On very small or unusual pages
    // it can lock onto a single stray node; falling back to the whole document
    // lets the chrome filter decide instead.
    let content = if extracted.len() < 512 && html.len() > extracted.len() * 2 {
        html.to_string()
    } else {
        extracted
    };
    let blocks = blocks_from_html(&content);

    if blocks.is_empty() {
        let kind = if looks_like_js_shell(html) {
            FailureKind::JsOnly
        } else {
            FailureKind::Unsupported
        };
        return Err(WebError::new(
            kind,
            format!("no readable blocks in {} bytes of HTML", html.len()),
        ));
    }

    if meta.title.trim().is_empty() {
        meta.title = url.to_string();
    }
    if meta.canonical.is_none() {
        meta.canonical = Some(url.to_string());
    }

    Ok(Page {
        url: url.to_string(),
        meta,
        blocks,
    })
}

/// A document that is already text (JSON, plain text, markdown).
pub fn single_block(url: &str, kind: BlockKind, text: String) -> Page {
    let id = block_id("", &text);
    Page {
        url: url.to_string(),
        meta: PageMeta {
            title: url.to_string(),
            ..Default::default()
        },
        blocks: vec![Block {
            id,
            kind,
            heading: String::new(),
            text,
        }],
    }
}

fn readable(html: &str, url: &str) -> (String, PageMeta) {
    let config = dom_smoothie::Config {
        max_elements_to_parse: 25_000,
        ..Default::default()
    };

    let Ok(mut readability) = dom_smoothie::Readability::new(html, Some(url), Some(config)) else {
        return (html.to_string(), PageMeta::default());
    };
    let Ok(article) = readability.parse() else {
        return (html.to_string(), PageMeta::default());
    };

    let meta = PageMeta {
        title: article.title.to_string(),
        byline: opt(article.byline),
        site_name: opt(article.site_name),
        published: opt(article.published_time),
        modified: opt(article.modified_time),
        canonical: opt(article.url),
    };
    (article.content.to_string(), meta)
}

fn opt<T: AsRef<str>>(value: Option<T>) -> Option<String> {
    value
        .map(|v| v.as_ref().trim().to_string())
        .filter(|v| !v.is_empty())
}

fn blocks_from_html(html: &str) -> Vec<Block> {
    let document = Html::parse_document(html);
    let mut blocks: Vec<Block> = Vec::new();
    let mut headings: Vec<(u8, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for node in document.tree.root().descendants() {
        let Node::Element(element) = node.value() else {
            continue;
        };
        let tag = element.name();
        if !BLOCK_TAGS.contains(&tag) {
            continue;
        }

        // Text inside chrome, or inside another block we already emitted, is
        // a duplicate: skip it rather than pay for it twice.
        let nested = node.ancestors().skip(1).any(|ancestor| match ancestor.value() {
            Node::Element(inner) => {
                CHROME_TAGS.contains(&inner.name()) || BLOCK_TAGS.contains(&inner.name())
            }
            _ => false,
        });
        if nested {
            continue;
        }

        let Some(element_ref) = ElementRef::wrap(node) else {
            continue;
        };

        let raw = element_ref.text().collect::<String>();
        let (kind, text) = match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = tag[1..].parse::<u8>().unwrap_or(1);
                let text = normalize(&raw);
                if text.is_empty() {
                    continue;
                }
                while headings.last().is_some_and(|(seen, _)| *seen >= level) {
                    headings.pop();
                }
                headings.push((level, text.clone()));
                (BlockKind::Heading, text)
            }
            "pre" => (BlockKind::Code, normalize_code(&raw)),
            "ul" | "ol" => (BlockKind::List, render_list(&element_ref)),
            "table" => (BlockKind::Table, render_table(&element_ref)),
            "blockquote" => (BlockKind::Quote, normalize(&raw)),
            "dl" => (BlockKind::List, normalize(&raw)),
            _ => (BlockKind::Paragraph, normalize(&raw)),
        };

        if text.trim().len() < 2 {
            continue;
        }

        // A paragraph that only repeats its own heading is the page echoing
        // itself, not evidence.
        let repeats_heading = kind == BlockKind::Paragraph
            && headings
                .last()
                .is_some_and(|(_, current)| normalize(current) == text);
        if repeats_heading {
            continue;
        }

        let path = heading_path(&headings);
        let id = block_id(&path, &text);
        // Two identical sentences under the same heading are one anchor.
        if !seen.insert(id.clone()) {
            continue;
        }
        blocks.push(Block {
            id,
            kind,
            heading: path,
            text,
        });

        if blocks.len() >= MAX_BLOCKS {
            break;
        }
    }

    blocks
}

fn heading_path(headings: &[(u8, String)]) -> String {
    headings
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

fn render_list(root: &ElementRef) -> String {
    let mut lines = Vec::new();
    for node in root.descendants() {
        let Node::Element(element) = node.value() else {
            continue;
        };
        if element.name() != "li" {
            continue;
        }
        let depth = node
            .ancestors()
            .filter(|ancestor| {
                matches!(ancestor.value(), Node::Element(inner) if inner.name() == "ul" || inner.name() == "ol")
            })
            .count()
            .saturating_sub(1);
        let Some(item) = ElementRef::wrap(node) else {
            continue;
        };
        let text = normalize(&own_text(&item));
        if !text.is_empty() {
            lines.push(format!("{}- {text}", "  ".repeat(depth)));
        }
    }
    lines.join("\n")
}

/// A list item's own prose, without the text of nested lists: those become
/// their own lines at a deeper indent.
fn own_text(element: &ElementRef) -> String {
    let mut out = String::new();
    for child in element.children() {
        match child.value() {
            Node::Text(text) => out.push_str(text),
            Node::Element(inner) if inner.name() != "ul" && inner.name() != "ol" => {
                if let Some(element) = ElementRef::wrap(child) {
                    out.push_str(&element.text().collect::<String>());
                }
            }
            _ => {}
        }
    }
    out
}

fn render_table(root: &ElementRef) -> String {
    let row_selector = scraper::Selector::parse("tr").expect("static selector");
    let cell_selector = scraper::Selector::parse("td, th").expect("static selector");
    let mut rows = Vec::new();

    for row in root.select(&row_selector) {
        let mut cells = Vec::new();
        for cell in row.select(&cell_selector) {
            let text = normalize(&cell.text().collect::<String>());
            if !text.is_empty() {
                cells.push(text);
            }
        }
        if !cells.is_empty() {
            rows.push(cells.join(" | "));
        }
    }
    rows.join("\n")
}

/// Collapse prose whitespace without joining words across element boundaries.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn normalize_code(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// Content-derived, so the same block keeps the same anchor across fetches.
/// The heading path is part of the hash: two identical sentences in different
/// sections are different evidence.
fn block_id(heading: &str, text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(heading.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(text.as_bytes());
    let hex = hasher.finalize().to_hex();
    format!("blk_{}", &hex.as_str()[..10])
}

fn looks_like_js_shell(html: &str) -> bool {
    const MARKERS: &[&str] = &[
        "__NEXT_DATA__",
        "__NUXT__",
        "window.__INITIAL_STATE__",
        "id=\"root\"",
        "id=\"app\"",
        "id=\"__next\"",
    ];
    let scripts = html.matches("<script").count();
    MARKERS.iter().any(|marker| html.contains(marker)) || scripts >= 3
}
