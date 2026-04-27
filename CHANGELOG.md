# Changelog

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
