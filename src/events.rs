use crate::types::{CliName, RunResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unified stream event emitted by all adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamEvent {
    /// Incremental text output.
    TextDelta { text: String },

    /// Incremental reasoning/thinking output.
    ThinkingDelta { text: String },

    /// A tool call has started.
    ToolStart {
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolId")]
        tool_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<HashMap<String, serde_json::Value>>,
    },

    /// A tool call has completed.
    ToolEnd {
        #[serde(rename = "toolId")]
        tool_id: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// A full agent turn has completed.
    TurnEnd,

    /// An error or warning.
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        severity: Option<Severity>,
    },

    /// The run has completed. Contains the final result.
    Done { result: RunResult },

    /// Escape hatch for provider-specific events.
    Raw {
        provider: CliName,
        event: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Severity {
    Warning,
    Error,
}
