//! Deterministic summarization of `cargo` output.
//!
//! The compiler already emits structured diagnostics, so nothing here infers
//! meaning from prose: [`parse_cargo`] reads `--message-format=json` records and
//! [`parse_libtest`] slices libtest's delimited text. The point is that the
//! agent never receives the raw log — only the grouped, budgeted report.

pub mod cache;
pub mod exec_tool;
pub mod parse_cargo;
pub mod parse_libtest;
pub mod runner;
pub mod schema;

pub use cache::SourceHashes;
pub use exec_tool::{ExecParams, ExecTool};
pub use runner::Kind;
pub use schema::brain_exec_tool;
