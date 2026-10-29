use serde::{Deserialize, Serialize};

/// What the page says about itself. Kept next to the blocks so a caller can
/// answer "when was this published" without reading prose.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMeta {
    pub title: String,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub published: Option<String>,
    pub modified: Option<String>,
    pub canonical: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Heading,
    Paragraph,
    List,
    Table,
    Code,
    Quote,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heading => "heading",
            Self::Paragraph => "paragraph",
            Self::List => "list",
            Self::Table => "table",
            Self::Code => "code",
            Self::Quote => "quote",
        }
    }
}

/// The addressable unit of a page.
///
/// `id` is derived from the block's own text and its heading path, so the same
/// content keeps the same anchor across fetches. That is what makes "only tell
/// me what changed" and `url#blk_xxxxxxxx` citations possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
    pub heading: String,
    pub text: String,
}

impl Block {
    pub fn anchor(&self) -> String {
        format!("#{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub url: String,
    pub meta: PageMeta,
    pub blocks: Vec<Block>,
}
