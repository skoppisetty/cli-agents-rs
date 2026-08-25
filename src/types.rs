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

/// Filesystem setting sources Claude Code loads at startup.
///
/// Maps to the `--setting-sources` CLI flag. When omitted, the Claude CLI
/// loads all three (user, project, local) and fires their hooks.
/// Pass `Some(vec![])` to skip all of them — useful when embedding the CLI
/// in another app that doesn't want global SessionStart hooks running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    User,
    Project,
    Local,
}

impl SettingSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

// ── Provider-specific options ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOptions {
    pub allowed_tools: Option<String>,
    pub disallowed_tools: Option<String>,
    pub tools: Option<String>,
    pub append_system_prompt: Option<String>,
    /// Path to a file whose contents are appended to the system prompt
    /// (`--append-system-prompt-file`). Takes precedence over the inline
    /// `append_system_prompt`. When only the inline form is given, the
    /// adapter writes it to a temp file and uses this flag anyway: a director
    /// prompt of a few tens of KB blows past Windows' 32 K command-line limit
    /// as an argument (`failed to spawn claude: The filename or extension is
    /// too long (os error 206)`), and a file has no such limit on any OS.
    pub append_system_prompt_file: Option<String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub max_thinking_tokens: Option<u32>,
    pub continue_session: Option<bool>,
    pub include_partial_messages: Option<bool>,
    pub effort: Option<String>,
    pub agents: Option<serde_json::Value>,
    /// Filesystem settings the CLI loads (and fires hooks for).
    /// `None` = let the CLI default (loads user/project/local).
    /// `Some(vec![])` = load nothing — silences global SessionStart hooks.
    pub setting_sources: Option<Vec<SettingSource>>,
    /// Extra arguments appended verbatim to the Claude CLI invocation.
    ///
    /// Useful for flags this crate does not model explicitly — for example
    /// `--json-schema <schema>` to force structured output, or
    /// `--input-format text`. Order is preserved; entries are appended after
    /// all flags this crate emits, so they can override earlier defaults.
    pub extra_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOptions {
    pub approval_policy: Option<String>,
    pub sandbox_mode: Option<String>,
    pub additional_directories: Option<Vec<String>>,
    pub images: Option<Vec<String>>,
    pub output_schema: Option<String>,
    /// Extra arguments appended verbatim to the Codex CLI invocation.
    ///
    /// Same semantics as the Claude / Gemini equivalents — pass any flags
    /// this crate doesn't model. Appended last, so they can override earlier
    /// defaults.
    pub extra_args: Option<Vec<String>>,
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

    /// Maximum bytes a single stdout LINE may retain in memory.
    ///
    /// Not a cumulative cap: streamed output is handed to the consumer
    /// line-by-line and never held, so total volume is unbounded by design —
    /// a long agent run is not an error. A line over the cap is dropped and
    /// surfaced as a warning event; the run continues. Defaults to 10 MB
    /// when `None`.
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
    /// The process's exit status — `None` when a SIGNAL ended it, or when the
    /// run never reached a process at all (cancellation).
    ///
    /// Do NOT read this as a diagnosis on its own: a CLI that reports failure
    /// through its own event stream can exit 0, and in earlier versions a
    /// signalled process was reported here as `1`. Prefer `success`, then
    /// `text`, and consult `signal` before blaming the code.
    pub exit_code: Option<i32>,
    /// The signal that terminated the process, when one did. Unix only.
    ///
    /// `Some(9)` is the usual shape of an out-of-memory kill — the case that is
    /// otherwise invisible, because a signalled process writes no stderr and
    /// emits no final event.
    pub signal: Option<i32>,
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
