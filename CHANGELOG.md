# Changelog

## 0.2.18

### Fixed
- **Claude: a long appended system prompt no longer fails to spawn on Windows.** `append_system_prompt` was passed as an argument; a director-sized prompt (tens of KB) exceeded the 32 K command-line limit and the run died before it started with `failed to spawn claude: The filename or extension is too long (os error 206)`. The adapter now writes the inline prompt to a temp file for the lifetime of the spawn and passes `--append-system-prompt-file` (Claude Code ≥ 2.1), on every OS.

### Added
- `ClaudeOptions.append_system_prompt_file` (`appendSystemPromptFile`): supply your own file; it takes precedence over the inline form and is never spilled.

## 0.2.14

### Fixed
- **A long run is no longer killed when cumulative stdout crosses `max_output_bytes`.** The cap was enforced against total throughput, but streamed lines are handed to the consumer and released — the total never lived in memory. A healthy agent run streaming tens of MB of events (a long agentic turn with large tool results) died at the 10 MB mark with its entire turn discarded: `Process("output exceeded max buffer size")`, and no `Done` event. `max_output_bytes` now bounds what a single LINE may retain: an oversized line is consumed to its newline, dropped, counted, and surfaced as a `Warning` error event; the run continues and always reaches `Done`.
- stderr is retained as a bounded 64 KiB tail (chunked reads, so a single unterminated line cannot grow it either) instead of in full. The tail is where the parting words that explain a failure live; a CLI logging megabytes to stderr no longer grows memory without bound.

### Notes
- Consumers that matched the `output exceeded max buffer size` error string will no longer see it — an over-limit line is a warning event and a dropped event, not a dead run.

## 0.2.12

### Added
- `RunResult::signal` — the signal that terminated the CLI process, when one did (unix only). `Some(9)` is the usual shape of an out-of-memory kill, a case that is otherwise invisible: a signalled process writes no stderr and emits no final event, so previously the only trace was a fabricated exit code.
- A signalled run with no other output now gets a readable `text` (e.g. "The agent was killed (SIGKILL), most often by the system reclaiming memory.") instead of nothing.

### Fixed
- **`RunResult::exit_code` no longer fabricates `1` for a process that was terminated by a signal.** `spawn_and_stream` finished with `status.code().unwrap_or(1)`, so a SIGKILL was indistinguishable from a CLI that cleanly exited 1 — and since `exit_code` is an `Option` precisely to express "no code", the substitution destroyed the only channel that could have carried the difference. A consumer seeing `exit_code: Some(1)` would go looking for an explanation in a stderr the killed process never got a chance to write. It is now `None` when a signal ended the run, with the signal itself reported alongside.

### Notes
- `RunResult` is `#[non_exhaustive]` and derives `Default`, so the new field is additive for downstream consumers.
- Read `success` first, then `text`, and consult `signal` before attributing anything to `exit_code`: a CLI that reports failure through its own event stream can still exit 0.

## 0.2.11

### Added
- `ClaudeOptions::setting_sources` — controls Claude's `--setting-sources` flag. Pass `Some(vec![])` to skip user/project/local settings and silence global `SessionStart` hooks when embedding the CLI in another app.
- `SettingSource` enum (`User`, `Project`, `Local`) re-exported from the crate root.
- Claude adapter strips `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` from the inherited environment before spawning, so the CLI uses its subscription credentials (OAuth/keychain) instead of silently falling back to API-token billing when the host process happens to have those env vars set. Callers can re-supply them explicitly via `RunOptions::env` if needed.

### Fixed
- Codex adapter no longer wipes the user's auth and history when MCP servers or a system prompt are configured. Previously, the temporary `CODEX_HOME` contained only the synthesized config, so the spawned CLI ran without credentials. The temp dir now symlinks the real `~/.codex` entries (auth, sessions, history) and writes a merged `config.toml` that overlays the new fields onto existing keys.

## 0.2.0

### Added
- npm binary distribution (`@cueframe/cli-agents`) with prebuilt binaries for macOS, Linux, and Windows
- Process group isolation: CLI subprocesses spawn in their own process group for clean shutdown
- GitHub Releases with downloadable binaries for all platforms

### Fixed
- Streaming tool input accumulation: use block index for correct delta routing instead of HashMap insertion order
- `content_block_stop` now only drains the completed tool, not all pending tools
- `setpgid` return value is now checked in the pre_exec closure
- Ctrl+C signal handler only installs when stdin is a terminal

### Changed
- `libc` is now a unix-only dependency
- Default features reverted to `[]` (library consumers no longer pull in `clap`)
- Process cleanup kills the entire process group, not just the child process

## 0.1.0

Initial release: unified Rust interface for Claude Code, Codex, and Gemini CLIs.

- `run()` API with streaming `StreamEvent` enum
- Auto-discovery of installed CLIs (PATH, nvm, homebrew)
- Timeouts, idle limits, and consecutive tool-failure guards
- MCP server configuration
- Async/Tokio with cancellation via `CancellationToken`
- Optional CLI binary behind `cli` feature flag
