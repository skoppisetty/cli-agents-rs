# Tauri + cli-agents example

A reference implementation showing how to integrate `cli-agents` into a Tauri desktop app.

## What it does

- Exposes a `run_cli_agent` Tauri command that accepts `RunOptions` as JSON
- Streams `StreamEvent`s to the frontend via Tauri events (`cli-agent-stdout`)
- Supports cancellation via a `cancel_cli_agent` command
- Emits `cli-agent-done` when the run completes

## Usage

This is not a runnable example — it depends on `tauri`, `specta`, and `log` which are not dependencies of this crate. Copy [`bridge.rs`](bridge.rs) into your Tauri project and adapt it.

### Frontend integration

```typescript
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

// Listen for streaming events
await listen('cli-agent-stdout', (event) => {
  const streamEvent = JSON.parse(event.payload);
  // Handle TextDelta, ToolStart, ToolEnd, Done, etc.
});

// Start a run
const result = await invoke('run_cli_agent', {
  config: JSON.stringify({
    task: "Review this codebase for security issues",
    skipPermissions: true,
  }),
  env: {},
});

// Cancel if needed
await invoke('cancel_cli_agent');
```

### Required dependencies in your Tauri project

```toml
[dependencies]
cli-agents = "0.2"
tauri = { version = "2", features = [...] }
specta = "..."
log = "0.4"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync"] }
```
