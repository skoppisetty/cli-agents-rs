mod parse;

use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub struct CodexAdapter;

impl CliAdapter for CodexAdapter {
    fn name(&self) -> CliName {
        CliName::Codex
    }

    async fn run(
        &self,
        opts: &RunOptions,
        emit: &(dyn Fn(StreamEvent) + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<RunResult> {
        let binary = match &opts.executable_path {
            Some(p) => p.clone(),
            None => discover_binary(CliName::Codex).await.ok_or(Error::NoCli)?,
        };

        // Write temp config if MCP servers or system_prompt_file are set.
        // Hold the TempDir so it lives until the child process exits.
        let (config_env, _tmp_dir) = write_configs(opts).await?;

        let args = build_args(opts);
        let mut extra_env = opts.env.clone().unwrap_or_default();
        extra_env.extend(config_env);
        let max_bytes = opts.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

        let mut state = parse::ParseState::default();
        let mut text_tracker: HashMap<String, String> = HashMap::new();

        let outcome = crate::adapters::spawn_and_stream(
            crate::adapters::SpawnParams {
                cli_label: "codex",
                binary: &binary,
                args: &args,
                extra_env: &extra_env,
                clear_env: opts.clear_env,
                strip_env: &[],
                cwd: opts.cwd.as_deref().unwrap_or("."),
                max_bytes,
                cancel: &cancel,
            },
            |line| parse::parse_line(line, &mut state, &mut text_tracker, emit),
        )
        .await?;

        match outcome {
            crate::adapters::SpawnOutcome::Cancelled => Ok(RunResult {
                success: false,
                text: Some("Cancelled.".into()),
                ..Default::default()
            }),
            crate::adapters::SpawnOutcome::Done {
                exit_code,
                signal,
                stderr,
                dropped_lines,
            } => {
                crate::adapters::warn_dropped_lines(dropped_lines, max_bytes, emit);
                let success = !state.failed && exit_code == Some(0);
                let text = if !success && state.result_text.is_none() {
                    crate::adapters::extract_error_message(stderr.as_deref())
                        .or_else(|| crate::adapters::describe_signal(signal))
                } else {
                    state.result_text
                };
                Ok(RunResult {
                    success,
                    text,
                    exit_code,
                    signal,
                    stats: state.stats,
                    session_id: state.session_id,
                    stderr,
                    cost_usd: None,
                })
            }
        }
    }
}

fn build_args(opts: &RunOptions) -> Vec<String> {
    let mut args = vec!["exec".into()];

    if let Some(cwd) = &opts.cwd {
        args.push("-C".into());
        args.push(cwd.clone());
    }

    // Resume a previous session if requested
    if let Some(session_id) = &opts.resume_session_id {
        args.push("resume".into());
        args.push(session_id.clone());
    }

    args.push(opts.task.clone());
    args.push("--json".into());

    if let Some(model) = &opts.model {
        args.push("--model".into());
        args.push(model.clone());
    }

    let codex_opts = opts.providers.as_ref().and_then(|p| p.codex.as_ref());

    if let Some(co) = codex_opts {
        if let Some(policy) = &co.approval_policy {
            match policy.as_str() {
                "full-auto" => args.push("--full-auto".into()),
                "suggest" | "auto-edit" => {
                    // Default Codex behavior — no flag needed
                }
                other => {
                    warn!(policy = other, "unknown Codex approval policy, ignoring");
                }
            }
        }
        if let Some(sandbox) = &co.sandbox_mode {
            args.push("--sandbox".into());
            args.push(sandbox.clone());
        }
        if let Some(dirs) = &co.additional_directories {
            for dir in dirs {
                args.push("-C".into());
                args.push(dir.clone());
            }
        }
        if let Some(images) = &co.images {
            for img in images {
                args.push("--image".into());
                args.push(img.clone());
            }
        }
        if let Some(schema) = &co.output_schema {
            args.push("--output-schema".into());
            args.push(schema.clone());
        }
    }

    // Permission bypass for non-interactive use (opt-in).
    // Skip if an explicit approval_policy is set — the two flags conflict.
    let has_policy = codex_opts
        .and_then(|c| c.approval_policy.as_deref())
        .is_some_and(|p| !p.is_empty());
    if opts.skip_permissions && !has_policy {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    }

    // Programmatic callers often set cwd to a non-git directory.
    if opts.skip_permissions {
        args.push("--skip-git-repo-check".into());
    }

    // User-supplied flags last, so callers can override earlier defaults.
    if let Some(extra) = codex_opts.and_then(|c| c.extra_args.as_ref()) {
        args.extend(extra.iter().cloned());
    }

    args
}

// ── Codex TOML config types ──

#[derive(Serialize)]
struct CodexConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_servers: Option<HashMap<String, CodexMcpServer>>,
}

#[derive(Serialize)]
struct CodexMcpServer {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_timeout_sec: Option<u64>,
}

/// Write temporary Codex config files for MCP servers and system prompts.
///
/// Codex reads MCP configuration from `config.toml` and system prompts from
/// an `instructions` field in the same file. We write a temporary config and
/// point Codex to it via `CODEX_HOME`.
///
/// Returns the env vars to set and the temp dir handle (must be kept alive
/// until the child process exits).
async fn write_configs(
    opts: &RunOptions,
) -> Result<(HashMap<String, String>, Option<tempfile::TempDir>)> {
    let has_mcp = opts.mcp_servers.as_ref().is_some_and(|s| !s.is_empty());
    let system_prompt = resolve_system_prompt(opts).await?;

    if !has_mcp && system_prompt.is_none() {
        return Ok((HashMap::new(), None));
    }

    let tmp_dir = crate::artifacts::temp_dir(opts, "cli-agents-codex-")?;
    let codex_home = resolve_codex_home(opts);
    if let Some(home) = &codex_home {
        link_codex_home_contents(home, tmp_dir.path())?;
    }

    let config = CodexConfig {
        instructions: system_prompt,
        mcp_servers: opts.mcp_servers.as_ref().map(|servers| {
            servers
                .iter()
                .map(|(name, s)| {
                    (
                        name.clone(),
                        CodexMcpServer {
                            command: s.command.clone(),
                            args: s.args.clone(),
                            env: s.env.clone(),
                            cwd: s.cwd.clone(),
                            tool_timeout_sec: s.timeout,
                        },
                    )
                })
                .collect()
        }),
    };

    let existing_config = codex_home.as_ref().map(|h| h.join("config.toml"));
    let config_table = merge_config(existing_config.as_deref(), config)?;
    let toml_str = toml::to_string_pretty(&config_table)
        .map_err(|e| Error::Other(format!("TOML serialization: {e}")))?;
    let config_path = tmp_dir.path().join("config.toml");
    tokio::fs::write(&config_path, toml_str)
        .await
        .map_err(Error::Io)?;

    let mut env = HashMap::new();
    env.insert(
        "CODEX_HOME".into(),
        tmp_dir.path().to_string_lossy().into_owned(),
    );
    Ok((env, Some(tmp_dir)))
}

fn resolve_codex_home(opts: &RunOptions) -> Option<PathBuf> {
    let configured = opts.env.as_ref().and_then(|env| {
        env.get("CODEX_HOME").map(PathBuf::from).or_else(|| {
            env.get("HOME")
                .map(|home| PathBuf::from(home).join(".codex"))
        })
    });
    if configured.is_some() || opts.artifact_dir.is_some() {
        return configured;
    }

    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// Symlink top-level entries of `src` into `dst`, skipping `config.toml`
/// (we write our own merged version). Symlinks avoid copying potentially
/// large session/history directories on every run.
fn link_codex_home_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(src).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        if entry.file_name() == "config.toml" {
            continue;
        }
        let target = dst.join(entry.file_name());
        symlink_entry(&entry.path(), &target)?;
    }

    Ok(())
}

#[cfg(unix)]
fn symlink_entry(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst).map_err(Error::Io)
}

#[cfg(windows)]
fn symlink_entry(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst).map_err(Error::Io)
    } else {
        std::os::windows::fs::symlink_file(src, dst).map_err(Error::Io)
    }
}

fn merge_config(existing_config: Option<&Path>, config: CodexConfig) -> Result<toml::Table> {
    let mut table = match existing_config {
        Some(path) if path.exists() => {
            let existing = std::fs::read_to_string(path).map_err(Error::Io)?;
            toml::from_str::<toml::Table>(&existing)
                .map_err(|e| Error::Other(format!("TOML parse: {e}")))?
        }
        _ => toml::Table::new(),
    };

    if let Some(instructions) = config.instructions {
        table.insert("instructions".into(), toml::Value::String(instructions));
    }

    if let Some(mcp_servers) = config.mcp_servers {
        let value = toml::Value::try_from(mcp_servers)
            .map_err(|e| Error::Other(format!("TOML conversion: {e}")))?;
        table.insert("mcp_servers".into(), value);
    }

    Ok(table)
}

/// Resolve the effective system prompt: `system_prompt_file` takes precedence
/// over `system_prompt`.
async fn resolve_system_prompt(opts: &RunOptions) -> Result<Option<String>> {
    if let Some(path) = &opts.system_prompt_file {
        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            Error::Process(format!("failed to read system prompt file {path}: {e}"))
        })?;
        Ok(Some(content))
    } else {
        Ok(opts.system_prompt.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_minimal() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"--json".to_string()));
    }

    #[test]
    fn build_args_no_permission_bypass_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn build_args_permission_bypass_when_opted_in() {
        let opts = RunOptions {
            task: "hello".into(),
            skip_permissions: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
    }

    #[test]
    fn build_args_resume_session() {
        let opts = RunOptions {
            task: "continue working".into(),
            resume_session_id: Some("tid-abc123".into()),
            cwd: Some("/tmp/project".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert_eq!(args[0..3], ["exec", "-C", "/tmp/project"]);
        let resume_idx = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume_idx + 1], "tid-abc123");
        assert_eq!(args[resume_idx + 2], "continue working");
    }

    #[test]
    fn build_args_full_auto() {
        let opts = RunOptions {
            task: "fix bug".into(),
            model: Some("o3".into()),
            providers: Some(crate::types::ProviderOptions {
                codex: Some(crate::types::CodexOptions {
                    approval_policy: Some("full-auto".into()),
                    sandbox_mode: Some("workspace-write".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--full-auto".to_string()));
        assert!(args.contains(&"--sandbox".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"o3".to_string()));
    }

    #[test]
    fn build_args_full_auto_with_skip_permissions_no_conflict() {
        let opts = RunOptions {
            task: "fix bug".into(),
            skip_permissions: true,
            providers: Some(crate::types::ProviderOptions {
                codex: Some(crate::types::CodexOptions {
                    approval_policy: Some("full-auto".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--full-auto".to_string()));
        assert!(
            !args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()),
            "should not pass both --full-auto and --dangerously-bypass-approvals-and-sandbox"
        );
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
    }

    #[tokio::test]
    async fn write_configs_creates_mcp_config() {
        let mut servers = HashMap::new();
        servers.insert(
            "test".into(),
            crate::types::McpServer {
                command: Some("test-server".into()),
                args: Some(vec!["--flag".into()]),
                ..Default::default()
            },
        );

        let opts = RunOptions {
            task: "hello".into(),
            mcp_servers: Some(servers),
            ..Default::default()
        };

        let (env, tmp_dir) = write_configs(&opts).await.unwrap();
        assert!(env.contains_key("CODEX_HOME"));
        let tmp = tmp_dir.unwrap();

        let config_path = tmp.path().join("config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("[mcp_servers.test]"));
        assert!(content.contains("test-server"));
    }

    #[tokio::test]
    async fn write_configs_with_system_prompt() {
        let opts = RunOptions {
            task: "hello".into(),
            system_prompt: Some("You are helpful.".into()),
            ..Default::default()
        };

        let (env, tmp_dir) = write_configs(&opts).await.unwrap();
        assert!(env.contains_key("CODEX_HOME"));
        let tmp = tmp_dir.unwrap();

        let config_path = tmp.path().join("config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("instructions"));
        assert!(content.contains("You are helpful."));
    }

    #[tokio::test]
    async fn generated_config_uses_the_owned_artifact_directory() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifact_dir.path().to_string_lossy().into_owned()),
            system_prompt: Some("owned prompt".into()),
            ..Default::default()
        };

        let (env, handle) = write_configs(&opts).await.unwrap();
        let codex_home = std::path::PathBuf::from(env.get("CODEX_HOME").unwrap());

        assert!(codex_home.starts_with(artifact_dir.path()));
        drop(handle);
    }

    #[test]
    fn owned_artifacts_never_import_the_ambient_codex_home() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifact_dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };

        assert!(resolve_codex_home(&opts).is_none());
    }

    #[test]
    fn owned_artifacts_use_only_the_explicit_child_codex_home() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let isolated_home = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifact_dir.path().to_string_lossy().into_owned()),
            env: Some(HashMap::from([(
                "HOME".into(),
                isolated_home.path().to_string_lossy().into_owned(),
            )])),
            ..Default::default()
        };

        assert_eq!(
            resolve_codex_home(&opts),
            Some(isolated_home.path().join(".codex"))
        );
    }

    #[tokio::test]
    async fn write_configs_noop_when_empty() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };

        let (env, tmp_dir) = write_configs(&opts).await.unwrap();
        assert!(env.is_empty());
        assert!(tmp_dir.is_none());
    }

    #[test]
    fn merge_config_preserves_existing_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"model = "o3"

[mcp_servers.preexisting]
command = "old-server"

[other_table]
foo = "bar"
"#,
        )
        .unwrap();

        let mut servers = HashMap::new();
        servers.insert(
            "test".into(),
            CodexMcpServer {
                command: Some("test-server".into()),
                args: None,
                env: None,
                cwd: None,
                tool_timeout_sec: None,
            },
        );

        let config = CodexConfig {
            instructions: Some("hello".into()),
            mcp_servers: Some(servers),
        };

        let merged = merge_config(Some(&path), config).unwrap();

        assert_eq!(merged.get("model").and_then(|v| v.as_str()), Some("o3"));
        assert!(merged.get("other_table").is_some());
        assert_eq!(
            merged.get("instructions").and_then(|v| v.as_str()),
            Some("hello")
        );
        // mcp_servers replaced wholesale — pre-existing entries are dropped
        // in favor of the caller-supplied set, which is the documented behavior.
        let mcp = merged
            .get("mcp_servers")
            .and_then(|v| v.as_table())
            .unwrap();
        assert!(mcp.contains_key("test"));
        assert!(!mcp.contains_key("preexisting"));
    }

    #[test]
    fn merge_config_no_existing_file() {
        let merged = merge_config(
            None,
            CodexConfig {
                instructions: Some("hi".into()),
                mcp_servers: None,
            },
        )
        .unwrap();
        assert_eq!(
            merged.get("instructions").and_then(|v| v.as_str()),
            Some("hi")
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_codex_home_skips_config_and_symlinks_rest() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("config.toml"), "model = \"old\"").unwrap();
        std::fs::write(src.path().join("auth.json"), "{\"token\":\"x\"}").unwrap();
        std::fs::create_dir(src.path().join("sessions")).unwrap();
        std::fs::write(src.path().join("sessions").join("a.jsonl"), "{}").unwrap();

        let dst = tempfile::tempdir().unwrap();
        link_codex_home_contents(src.path(), dst.path()).unwrap();

        assert!(!dst.path().join("config.toml").exists());
        let auth = dst.path().join("auth.json");
        assert!(auth.exists());
        assert!(auth.symlink_metadata().unwrap().file_type().is_symlink());
        let sessions = dst.path().join("sessions");
        assert!(
            sessions
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // Reading through the symlink reaches the original file.
        let contents =
            std::fs::read_to_string(dst.path().join("sessions").join("a.jsonl")).unwrap();
        assert_eq!(contents, "{}");
    }

    #[tokio::test]
    async fn write_configs_system_prompt_file_takes_precedence() {
        let fixture = tempfile::tempdir().unwrap();

        // Write a prompt file
        let prompt_file = fixture.path().join("prompt.md");
        std::fs::write(&prompt_file, "File prompt content").unwrap();

        let opts = RunOptions {
            task: "hello".into(),
            system_prompt: Some("Inline prompt".into()),
            system_prompt_file: Some(prompt_file.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let (env, tmp_dir) = write_configs(&opts).await.unwrap();
        assert!(env.contains_key("CODEX_HOME"));
        let tmp = tmp_dir.unwrap();

        let config_path = tmp.path().join("config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("File prompt content"));
        assert!(!content.contains("Inline prompt"));
    }

    #[test]
    fn build_args_extra_args_omitted_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.iter().any(|a| a == "--proto"));
    }

    #[test]
    fn build_args_extra_args_appended_verbatim() {
        let opts = RunOptions {
            task: "hello".into(),
            providers: Some(crate::types::ProviderOptions {
                codex: Some(crate::types::CodexOptions {
                    extra_args: Some(vec!["--proto".into(), "json".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        let idx = args
            .iter()
            .position(|a| a == "--proto")
            .expect("extra_args flag emitted");
        assert_eq!(args[idx + 1], "json");
    }

    #[test]
    fn build_args_extra_args_come_after_crate_defaults() {
        // Callers should be able to override what this crate emits — same
        // contract as the Claude adapter. User-supplied flags land last.
        let opts = RunOptions {
            task: "hello".into(),
            skip_permissions: true,
            providers: Some(crate::types::ProviderOptions {
                codex: Some(crate::types::CodexOptions {
                    extra_args: Some(vec!["--skip-git-repo-check".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        // The crate emits --skip-git-repo-check once for skip_permissions,
        // then the user's extra copy. The user copy must be strictly later.
        let positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| (a == "--skip-git-repo-check").then_some(i))
            .collect();
        assert_eq!(
            positions.len(),
            2,
            "both crate-emitted and user-supplied copies expected, got {args:?}"
        );
        // Last occurrence is the user-supplied one.
        assert!(positions[1] > positions[0]);
    }
}
