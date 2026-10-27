//! Token-economy benchmark for `brain-tools`.
//!
//! The harness replays a fixed set of questions over MCP stdio, measures what
//! the server actually returns, and compares it with what an agent without the
//! server would have paid: the full content of the files it would have opened.
//!
//! Three rules keep the number honest:
//!
//! 1. The measurement is the real response text from the real binary, never a
//!    model of it. The JSON-RPC envelope and the call arguments count too.
//! 2. The baseline is files the task names explicitly. A task may not declare
//!    an empty baseline, because a ratio against zero is not a measurement.
//! 3. Tools whose output depends on machine state (`brain_exec`, `brain_peek`,
//!    `brain_ledger`) are rejected: they cannot be replayed reproducibly.
//!
//! Token counts use the same estimator as the server's budget
//! (`brain_kernel::budget::estimate_tokens`), so "tokens" here and "budget
//! tokens" there mean the same thing.

pub mod client;
pub mod naive;
pub mod report;
pub mod tasks;

pub use report::{compare, Comparison, Regression, RunReport};
pub use tasks::{Call, Task, TaskSet};
