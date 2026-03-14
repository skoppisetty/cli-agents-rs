# cli-agents

[![Crates.io](https://img.shields.io/crates/v/cli-agents.svg)](https://crates.io/crates/cli-agents)
[![npm](https://img.shields.io/npm/v/@cueframe/cli-agents.svg)](https://www.npmjs.com/package/@cueframe/cli-agents)
[![CI](https://github.com/skoppisetty/cli-agents-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/skoppisetty/cli-agents-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Build agentic apps on top of your users' existing AI subscriptions.

Instead of requiring API keys or managing token costs, `cli-agents` spawns
the AI CLI tools users already have installed — Claude Code, Codex, or
Gemini CLI — and provides a unified Rust interface for streaming events,
tool calls, cancellation, and structured results. Your app brings the UX;
the user brings their own subscription.

## Features

- **Single API** — `run()` works with any supported CLI backend.
- **Auto-discovery** — finds installed CLIs in PATH, nvm, homebrew, etc.
- **Streaming events** — unified `StreamEvent` enum for text, thinking, tool
  calls, errors, and completion across all providers.
- **Timeouts & guardrails** — idle timeout, total timeout, and consecutive
  tool-failure limits with automatic cancellation.
- **MCP support** — configure Model Context Protocol servers for all adapters.
- **Async / Tokio** — fully async with cancellation via `CancellationToken`.

## Quick start

```rust
use cli_agents::{run, RunOptions, StreamEvent, CliName};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let opts = RunOptions {
        cli: Some(CliName::Claude),
        task: "List all public functions in src/lib.rs and describe what each one does.".into(),
        cwd: Some("./my-project".into()),
        skip_permissions: true,
        ..Default::default()
    };

    let handle = run(opts, Some(Arc::new(|event: StreamEvent| {
        match &event {
            StreamEvent::TextDelta { text } => print!("{text}"),
            StreamEvent::Done { result } => println!("\n\nDone: {:?}", result.success),
            _ => {}
        }
    })));

    let result = handle.result.await.unwrap().unwrap();
    println!("Success: {}", result.success);
}
```

## Installation

### As a Rust library

```toml
[dependencies]
cli-agents = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### As a CLI tool

```bash
# Via npm (recommended — prebuilt binaries, no Rust toolchain needed)
npm install -g @cueframe/cli-agents

# Via cargo
cargo install cli-agents --features cli

# Or download a binary from GitHub Releases
# https://github.com/skoppisetty/cli-agents-rs/releases
```

### Prerequisites

You need at least one supported AI CLI installed:

| CLI | Install |
|-----|---------|
| Claude Code | `npm install -g @anthropic-ai/claude-code` |
| Codex | `npm install -g @openai/codex` |
| Gemini CLI | `npm install -g @google/gemini-cli` |

## Auto-discovery

Omit `cli` to auto-discover the first available agent (Claude > Codex > Gemini):

```rust
let opts = RunOptions {
    task: "Read the README and give a one-paragraph summary of this project.".into(),
    cwd: Some("./my-project".into()),
    ..Default::default()
};
let handle = run(opts, None);
```

You can also query what's installed:

```rust
use cli_agents::discovery::{discover_all, discover_first};

let all = discover_all().await;
for (name, path) in &all {
    println!("{name}: {path}");
}
```

## Configuration

### System prompts

```rust
let opts = RunOptions {
    task: "Read src/ and identify any potential security issues or panics.".into(),
    cwd: Some("./my-project".into()),
    system_prompt: Some("You are a senior Rust reviewer. Be concise.".into()),
    // Or from a file:
    // system_prompt_file: Some("./prompts/reviewer.md".into()),
    ..Default::default()
};
```

### MCP servers

```rust
use cli_agents::{McpServer, RunOptions};
use std::collections::HashMap;

let mut servers = HashMap::new();
servers.insert("my-server".into(), McpServer {
    command: Some("npx".into()),
    args: Some(vec!["-y".into(), "my-mcp-server".into()]),
    ..Default::default()
});

let opts = RunOptions {
    task: "List the 5 most recent open issues.".into(),
    mcp_servers: Some(servers),
    ..Default::default()
};
```

### Timeouts and safety

```rust
let opts = RunOptions {
    task: "Run the test suite, fix any failures, and re-run until all tests pass.".into(),
    cwd: Some("/home/user/my-app".into()),
    skip_permissions: true,
    idle_timeout_ms: Some(60_000),          // 1 min idle timeout
    total_timeout_ms: Some(300_000),        // 5 min total timeout
    max_consecutive_tool_failures: Some(5), // Abort after 5 consecutive failures
    ..Default::default()
};
```

### Permission bypass

By default, the library does **not** bypass CLI permission prompts. Set
`skip_permissions: true` to pass flags like `--dangerously-skip-permissions`
(Claude) or `--dangerously-bypass-approvals-and-sandbox` (Codex).
**Use with caution** — the agent will execute tools without human confirmation.

### Provider-specific options

```rust
use cli_agents::{RunOptions, ProviderOptions, ClaudeOptions};

let opts = RunOptions {
    task: "Refactor the error handling in src/lib.rs to use thiserror.".into(),
    cwd: Some("/home/user/my-app".into()),
    skip_permissions: true,
    providers: Some(ProviderOptions {
        claude: Some(ClaudeOptions {
            max_turns: Some(10),
            max_thinking_tokens: Some(8000),
            ..Default::default()
        }),
        ..Default::default()
    }),
    ..Default::default()
};
```

## Cancellation

```rust
let handle = run(opts, None);

let cancel = handle.cancel.clone();
tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    cancel.cancel();
});

let result = handle.result.await.unwrap().unwrap();
// result.success == false when cancelled
```

## Why cli-agents?

AI APIs require API keys and charge per token. Most developers already pay for
Claude Pro, ChatGPT Plus, or Gemini — and those subscriptions come with CLI
tools. `cli-agents` lets you build desktop apps, dev tools, and automation
that run on the user's existing subscription with zero API cost to you.

This is especially powerful for **Tauri desktop apps**: your Rust backend
spawns the agent, streams events to the frontend, and the user's own
subscription handles the AI — no API keys to configure, no billing to manage.

## Use cases

### Desktop apps (Tauri)

The primary use case. Build rich agentic UIs that leverage whatever AI CLI
the user has installed. `RunOptions` is serde-compatible, so front ends pass
config as JSON and receive `StreamEvent`s back via Tauri events.

See [`tauri-example/`](tauri-example/) for a
complete Tauri integration with streaming and cancellation.

### Dev tools and CI

Build custom coding agents, review bots, or CI automation that piggyback on
existing CLI installations — no API keys needed.

### Multi-agent orchestration

Run multiple agents concurrently or route tasks to different providers based
on availability or capability.

## StreamEvent variants

| Variant | Description |
|---------|-------------|
| `TextDelta` | Incremental text output |
| `ThinkingDelta` | Reasoning / thinking output |
| `ToolStart` | A tool call has started |
| `ToolEnd` | A tool call has completed |
| `TurnEnd` | A full agent turn has completed |
| `Error` | An error or warning |
| `Done` | Run completed with final `RunResult` |
| `Raw` | Provider-specific event (escape hatch) |

## CLI binary

```bash
# Summarize a project (auto-discovers installed CLI)
cli-agents --cwd ./my-project "Read the README and summarize this project."

# Point at a codebase (auto-discovers CLI)
cli-agents --cwd ./my-project "Find all TODO comments and list them."

# Code review with a system prompt
cli-agents --cli codex --cwd ./my-project \
  --system "You are a senior code reviewer. Be concise." \
  "Review src/lib.rs for potential bugs."

# Stream events as JSON lines (useful for piping into other tools)
cli-agents --json --cwd ./my-project "List all public structs in src/"

# Verbose mode with Gemini (show tool calls, thinking, and token stats)
cli-agents --cli gemini -v --cwd ~/projects/my-app "What dependencies does this project have?"

# List available CLIs on this system
cli-agents --discover
```

## Platform support

| Platform | Status |
|----------|--------|
| macOS (ARM64, x64) | Fully tested end-to-end |
| Linux (x64, ARM64) | Compiles and passes unit tests in CI — not yet tested end-to-end. Feedback welcome! |
| Windows (x64) | Compiles and passes unit tests in CI — CLI discovery and process groups use Unix APIs and need platform-specific implementations. Contributions welcome! |

If you run into issues on Linux or Windows, please [open an issue](https://github.com/skoppisetty/cli-agents-rs/issues). The main platform-specific code is in `src/discovery.rs` (binary lookup) and `src/adapters/mod.rs` (process group handling).

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## License

MIT
