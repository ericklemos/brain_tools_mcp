//! `brain_fetch` and `brain_websearch`: the web reduced to addressable blocks.
//!
//! The tools are deliberately not `fetch` clones. They return *evidence*: every
//! block of the page carries a stable id, the response obeys a token budget,
//! and the overflow goes to the same `ResponseStore` that `brain_peek` pages.
//! Reading the same URL again revalidates instead of paying for the body twice.

pub mod cache;
pub mod error;
pub mod extract;
pub mod fetch_tool;
pub mod http;
pub mod model;
pub mod rank;
pub mod search;
pub mod util;

pub use error::{FailureKind, WebError};
pub use fetch_tool::FetchTool;
pub use model::{Block, BlockKind, Page, PageMeta};
pub use search::WebSearchTool;
