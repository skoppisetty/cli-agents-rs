use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported CLI agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CliName {
    Claude,
    Codex,
    Gemini,
}

impl std::fmt::Display for CliName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
            Self::Gemini => write!(f, "gemini"),
        }
    }
}

/// MCP server configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    // ── stdio transport ──
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub cwd: Option<String>,

    // ── HTTP/SSE transport ──
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub transport_type: Option<McpTransport>,
    pub headers: Option<HashMap<String, String>>,

    // ── Tool filtering ──
    pub include_tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,

    // ── Timeouts ──
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

// ── Provider-specific options ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOptions {
    pub allowed_tools: Option<String>,
    pub disallowed_tools: Option<String>,
    pub tools: Option<String>,
    pub append_system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub max_thinking_tokens: Option<u32>,
    pub continue_session: Option<bool>,
    pub include_partial_messages: Option<bool>,
    pub effort: Option<String>,
    pub agents: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOptions {
    pub approval_policy: Option<String>,
    pub sandbox_mode: Option<String>,
    pub additional_directories: Option<Vec<String>>,
    pub images: Option<Vec<String>>,
    pub output_schema: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiOptions {
    pub approval_mode: Option<String>,
    pub sandbox: Option<bool>,
    pub extra_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOptions {
    pub claude: Option<ClaudeOptions>,
    pub codex: Option<CodexOptions>,
    pub gemini: Option<GeminiOptions>,
}

/// Options passed to [`run()`](crate::run).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOptions {
    /// Which CLI to use. If `None`, auto-discovers the first available.
    pub cli: Option<CliName>,

    /// The task/prompt to send to the agent.
    pub task: String,

    /// System prompt (where supported).
    pub system_prompt: Option<String>,

    /// Path to a system prompt file (alternative to inline `system_prompt`).
    pub system_prompt_file: Option<String>,

    /// MCP servers to connect.
    pub mcp_servers: Option<HashMap<String, McpServer>>,

    /// Working directory for the CLI process.
    pub cwd: Option<String>,

    /// Model name (e.g. "sonnet", "opus", "o3").
    pub model: Option<String>,

    /// Idle timeout in milliseconds. Default: 300_000 (5 minutes).
    pub idle_timeout_ms: Option<u64>,

    /// Total timeout in milliseconds. No default.
    pub total_timeout_ms: Option<u64>,

    /// Max consecutive tool failures before aborting. Default: 3.
    pub max_consecutive_tool_failures: Option<u32>,

    /// Extra environment variables for the CLI process.
    pub env: Option<HashMap<String, String>>,

    /// Explicit path to the CLI executable (skips discovery).
    pub executable_path: Option<String>,

    /// Session ID to resume a previous conversation.
    pub resume_session_id: Option<String>,

    /// Maximum bytes to buffer from CLI stdout before aborting.
    ///
    /// Prevents OOM if the CLI produces unexpectedly large output.
    /// Defaults to 10 MB when `None`.
    pub max_output_bytes: Option<usize>,

    /// Skip permission prompts and run in fully autonomous mode.
    ///
    /// When `true`, passes provider-specific flags to bypass interactive approval
    /// (e.g. `--dangerously-skip-permissions` for Claude). **Use with caution** —
    /// the agent will be able to execute tools without human confirmation.
    ///
    /// Defaults to `false`.
    #[serde(default)]
    pub skip_permissions: bool,

    /// Provider-specific options.
    pub providers: Option<ProviderOptions>,
}

/// Result from a completed run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunResult {
    pub success: bool,
    pub text: Option<String>,
    pub exit_code: Option<i32>,
    pub stats: Option<RunStats>,
    pub session_id: Option<String>,
    pub stderr: Option<String>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunStats {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub tool_calls: Option<u32>,
}
