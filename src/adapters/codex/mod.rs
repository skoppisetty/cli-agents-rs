mod parse;

use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use serde::Serialize;
use std::collections::HashMap;
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
            crate::adapters::SpawnOutcome::Done { exit_code, stderr } => Ok(RunResult {
                success: !state.failed && exit_code == 0,
                text: state.result_text,
                exit_code: Some(exit_code),
                stats: state.stats,
                session_id: state.session_id,
                stderr,
                cost_usd: None,
            }),
        }
    }
}

fn build_args(opts: &RunOptions) -> Vec<String> {
    let mut args = vec!["exec".into()];

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

    if let Some(cwd) = &opts.cwd {
        args.push("--cd".into());
        args.push(cwd.clone());
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
                args.push("--cd".into());
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

    // Permission bypass for non-interactive use (opt-in)
    if opts.skip_permissions {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
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

    let tmp_dir = tempfile::tempdir().map_err(Error::Io)?;
    let codex_dir = tmp_dir.path().join(".codex");
    tokio::fs::create_dir_all(&codex_dir)
        .await
        .map_err(Error::Io)?;

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

    let toml_str = toml::to_string_pretty(&config)
        .map_err(|e| Error::Other(format!("TOML serialization: {e}")))?;

    let config_path = codex_dir.join("config.toml");
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
    }

    #[test]
    fn build_args_resume_session() {
        let opts = RunOptions {
            task: "continue working".into(),
            resume_session_id: Some("tid-abc123".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        // Should be: exec resume <session_id> <task> --json
        let resume_idx = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume_idx + 1], "tid-abc123");
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

        let config_path = tmp.path().join(".codex/config.toml");
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

        let config_path = tmp.path().join(".codex/config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("instructions"));
        assert!(content.contains("You are helpful."));
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

        let config_path = tmp.path().join(".codex/config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("File prompt content"));
        assert!(!content.contains("Inline prompt"));
    }
}
