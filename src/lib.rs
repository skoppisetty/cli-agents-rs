//! # cli-agents
//!
//! Build agentic apps on top of your users' existing AI subscriptions.
//!
//! Instead of requiring API keys or managing token costs, `cli-agents` spawns
//! the AI CLI tools users already have installed — Claude Code, Codex, or
//! Gemini CLI — and provides a unified interface for streaming events,
//! tool calls, cancellation, and structured results.
//!
//! ## Quick start
//!
//! ```no_run
//! use cli_agents::{run, RunOptions, StreamEvent, CliName};
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let opts = RunOptions {
//!         cli: Some(CliName::Claude),
//!         task: "What is 2+2?".into(),
//!         skip_permissions: true,
//!         ..Default::default()
//!     };
//!
//!     let handle = run(opts, Some(Arc::new(|event: StreamEvent| {
//!         match &event {
//!             StreamEvent::TextDelta { text } => print!("{text}"),
//!             StreamEvent::Done { result } => println!("\n\nDone: {:?}", result.success),
//!             _ => {}
//!         }
//!     })));
//!
//!     let result = handle.result.await.unwrap().unwrap();
//!     println!("Success: {}", result.success);
//! }
//! ```

pub mod adapters;
pub mod discovery;
pub mod error;
pub mod events;
pub mod runner;
pub mod types;

/// Default max stdout buffer size: 10 MB.
///
/// Shared across all adapters to prevent OOM if a CLI produces unexpectedly
/// large output. Override per-run via [`RunOptions::max_output_bytes`].
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

// Re-export primary API
pub use error::{Error, Result};
pub use events::{Severity, StreamEvent};
pub use runner::{RunHandle, run};
pub use types::{
    ClaudeOptions, CliName, CodexOptions, GeminiOptions, McpServer, McpTransport, ProviderOptions,
    RunOptions, RunResult, RunStats,
};
