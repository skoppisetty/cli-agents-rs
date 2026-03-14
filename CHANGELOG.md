# Changelog

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
