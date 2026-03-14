# @cueframe/cli-agents

[![npm](https://img.shields.io/npm/v/@cueframe/cli-agents.svg)](https://www.npmjs.com/package/@cueframe/cli-agents)
[![CI](https://github.com/skoppisetty/cli-agents-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/skoppisetty/cli-agents-rs/actions/workflows/ci.yml)

Prebuilt binaries for [cli-agents](https://github.com/skoppisetty/cli-agents-rs) — a unified interface for Claude Code, Codex, and Gemini CLIs.

## Install

```bash
npm install -g @cueframe/cli-agents
```

This installs the `cli-agents` binary for your platform. No Rust toolchain needed.

## Usage

```bash
# Auto-discovers installed CLI (Claude > Codex > Gemini)
cli-agents --cwd ./my-project "Summarize this project"

# Use a specific CLI
cli-agents --cli claude --cwd ./my-project "Review src/lib.rs for bugs"

# Stream events as JSON
cli-agents --json --cwd ./my-project "List all public functions"

# List available CLIs
cli-agents --discover
```

## Prerequisites

You need at least one AI CLI installed:

- [Claude Code](https://www.npmjs.com/package/@anthropic-ai/claude-code) — `npm install -g @anthropic-ai/claude-code`
- [Codex](https://www.npmjs.com/package/@openai/codex) — `npm install -g @openai/codex`
- [Gemini CLI](https://www.npmjs.com/package/@google/gemini-cli) — `npm install -g @google/gemini-cli`

## Platforms

| Platform | Package |
|----------|---------|
| macOS ARM64 | `@cueframe/cli-agents-darwin-arm64` |
| macOS x64 | `@cueframe/cli-agents-darwin-x64` |
| Linux x64 | `@cueframe/cli-agents-linux-x64` |
| Linux ARM64 | `@cueframe/cli-agents-linux-arm64` |
| Windows x64 | `@cueframe/cli-agents-win32-x64` |

The correct platform package is installed automatically via `optionalDependencies`.

## Programmatic usage (Node.js)

```js
const { binaryPath } = require("@cueframe/cli-agents");
console.log(binaryPath()); // /path/to/cli-agents binary
```

## Rust library

For Rust projects, use the crate directly: [cli-agents on crates.io](https://crates.io/crates/cli-agents)

## License

MIT
