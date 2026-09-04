mod parse;

use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, McpServer, RunOptions, RunResult};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

pub struct GeminiAdapter;

impl CliAdapter for GeminiAdapter {
    fn name(&self) -> CliName {
        CliName::Gemini
    }

    async fn run(
        &self,
        opts: &RunOptions,
        emit: &(dyn Fn(StreamEvent) + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<RunResult> {
        let binary = match &opts.executable_path {
            Some(p) => p.clone(),
            None => discover_binary(CliName::Gemini).await.ok_or(Error::NoCli)?,
        };

        // Write temp configs if needed.
        // Hold the TempDir so it lives until the child process exits.
        let (config_env, cwd_override, _tmp_dir) = write_configs(opts).await?;

        let cli_args = build_args(opts);
        let mut extra_env = opts.env.clone().unwrap_or_default();
        extra_env.extend(config_env);
        let max_bytes = opts.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

        // Use cwd override (temp dir with workspace MCP config) if set,
        // otherwise use the user-specified cwd.
        let effective_cwd = cwd_override
            .as_deref()
            .or(opts.cwd.as_deref())
            .unwrap_or(".");

        let mut state = parse::ParseState::default();

        let outcome = crate::adapters::spawn_and_stream(
            crate::adapters::SpawnParams {
                cli_label: "gemini",
                binary: &binary,
                args: &cli_args,
                extra_env: &extra_env,
                clear_env: opts.clear_env,
                strip_env: &[],
                cwd: effective_cwd,
                max_bytes,
                cancel: &cancel,
            },
            |line| parse::parse_line(line, &mut state, emit),
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
                let success = exit_code == Some(0);
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
    let mut args = vec![
        "-p".into(),
        opts.task.clone(),
        "--output-format".into(),
        "stream-json".into(),
    ];

    if let Some(model) = &opts.model {
        args.push("--model".into());
        args.push(model.clone());
    }

    // Gemini's --resume takes "latest" or an index, not a UUID session ID.
    // Use "latest" to resume the most recent session in the cwd.
    if opts.resume_session_id.is_some() {
        args.push("--resume".into());
        args.push("latest".into());
    }

    // Permission bypass for non-interactive use (opt-in)
    if opts.skip_permissions {
        args.push("--yolo".into());
    }

    if let Some(gemini) = opts.providers.as_ref().and_then(|p| p.gemini.as_ref()) {
        if gemini.sandbox == Some(true) {
            args.push("-s".into());
        }
        // Skip --approval-mode when --yolo is already set (they conflict in Gemini CLI).
        if !opts.skip_permissions {
            if let Some(mode) = &gemini.approval_mode {
                args.push("--approval-mode".into());
                args.push(mode.clone());
            }
        }
        if let Some(extra) = &gemini.extra_args {
            args.extend(extra.clone());
        }
    }

    args
}

/// Write temporary config files for MCP servers and system prompts.
///
/// Returns env vars, an optional cwd override, and the temp dir handle
/// (must be kept alive until the child process exits).
async fn write_configs(
    opts: &RunOptions,
) -> Result<(
    HashMap<String, String>,
    Option<String>,
    Option<tempfile::TempDir>,
)> {
    let has_mcp = opts.mcp_servers.as_ref().is_some_and(|s| !s.is_empty());
    let needs_prompt_file = opts.system_prompt_file.is_none() && opts.system_prompt.is_some();

    // system_prompt_file doesn't need a temp dir — it points to the file directly.
    if !has_mcp && !needs_prompt_file {
        let mut env = HashMap::new();
        if let Some(path) = &opts.system_prompt_file {
            env.insert("GEMINI_SYSTEM_MD".into(), path.clone());
        }
        return Ok((env, None, None));
    }

    let tmp_dir = crate::artifacts::temp_dir(opts, "cli-agents-gemini-")?;
    let mut env = HashMap::new();

    // MCP servers → generated settings. Sandboxed embedders receive the
    // config through Gemini's explicit system-settings override so the
    // requested cwd remains unchanged and no project file is overwritten.
    // Legacy callers without an artifact root retain workspace config behavior.
    let cwd_override = if let Some(servers) = &opts.mcp_servers {
        if !servers.is_empty() {
            let settings = build_mcp_settings(servers);
            let (settings_path, cwd_override) = if opts.artifact_dir.is_some() {
                (tmp_dir.path().join("settings.json"), None)
            } else {
                let config_dir = opts
                    .cwd
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| tmp_dir.path().to_path_buf());
                let gemini_dir = config_dir.join(".gemini");
                tokio::fs::create_dir_all(&gemini_dir)
                    .await
                    .map_err(Error::Io)?;
                let cwd_override = opts
                    .cwd
                    .is_none()
                    .then(|| tmp_dir.path().to_string_lossy().into_owned());
                (gemini_dir.join("settings.json"), cwd_override)
            };
            tokio::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)
                .await
                .map_err(Error::Io)?;
            if opts.artifact_dir.is_some() {
                env.insert(
                    "GEMINI_CLI_SYSTEM_SETTINGS_PATH".into(),
                    settings_path.to_string_lossy().into_owned(),
                );
            }
            cwd_override
        } else {
            None
        }
    } else {
        None
    };

    // System prompt → file referenced by GEMINI_SYSTEM_MD
    // system_prompt_file takes precedence (use the file directly).
    if let Some(path) = &opts.system_prompt_file {
        env.insert("GEMINI_SYSTEM_MD".into(), path.clone());
    } else if let Some(prompt) = &opts.system_prompt {
        let prompt_path = tmp_dir.path().join("system-prompt.md");
        tokio::fs::write(&prompt_path, prompt)
            .await
            .map_err(Error::Io)?;
        env.insert(
            "GEMINI_SYSTEM_MD".into(),
            prompt_path.to_string_lossy().into_owned(),
        );
    }

    Ok((env, cwd_override, Some(tmp_dir)))
}

fn build_mcp_settings(servers: &HashMap<String, McpServer>) -> serde_json::Value {
    let mut mcp_map = serde_json::Map::new();

    for (name, server) in servers {
        let mut entry = serde_json::Map::new();

        if let Some(url) = &server.url {
            entry.insert("url".into(), serde_json::Value::String(url.clone()));
            let t = match server.transport_type {
                Some(crate::types::McpTransport::Http) => "http",
                _ => "sse",
            };
            entry.insert("type".into(), serde_json::Value::String(t.into()));
            if let Some(headers) = &server.headers {
                entry.insert(
                    "headers".into(),
                    serde_json::to_value(headers).unwrap_or_default(),
                );
            }
        } else {
            if let Some(cmd) = &server.command {
                entry.insert("command".into(), serde_json::Value::String(cmd.clone()));
            }
            if let Some(a) = &server.args {
                entry.insert("args".into(), serde_json::to_value(a).unwrap_or_default());
            }
            if let Some(e) = &server.env {
                entry.insert("env".into(), serde_json::to_value(e).unwrap_or_default());
            }
            if let Some(cwd) = &server.cwd {
                entry.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
            }
        }

        if let Some(include) = &server.include_tools {
            entry.insert(
                "includeTools".into(),
                serde_json::to_value(include).unwrap_or_default(),
            );
        }
        if let Some(exclude) = &server.exclude_tools {
            entry.insert(
                "excludeTools".into(),
                serde_json::to_value(exclude).unwrap_or_default(),
            );
        }
        if let Some(timeout) = server.timeout {
            entry.insert("timeout".into(), serde_json::Value::Number(timeout.into()));
        }

        mcp_map.insert(name.clone(), serde_json::Value::Object(entry));
    }

    let mut root = serde_json::Map::new();
    root.insert("mcpServers".into(), serde_json::Value::Object(mcp_map));
    serde_json::Value::Object(root)
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
        assert_eq!(args, vec!["-p", "hello", "--output-format", "stream-json"]);
    }

    #[test]
    fn build_args_skip_permissions() {
        let opts = RunOptions {
            task: "hello".into(),
            skip_permissions: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn build_args_no_permission_bypass_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn build_args_with_options() {
        let opts = RunOptions {
            task: "do something".into(),
            model: Some("gemini-2.0-flash".into()),
            resume_session_id: Some("sess-1".into()),
            providers: Some(crate::types::ProviderOptions {
                gemini: Some(crate::types::GeminiOptions {
                    sandbox: Some(true),
                    approval_mode: Some("auto".into()),
                    extra_args: Some(vec!["--verbose".into()]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"gemini-2.0-flash".to_string()));
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"--approval-mode".to_string()));
        assert!(args.contains(&"--verbose".to_string()));
    }

    #[tokio::test]
    async fn generated_prompt_uses_the_owned_artifact_directory() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifact_dir.path().to_string_lossy().into_owned()),
            system_prompt: Some("owned prompt".into()),
            ..Default::default()
        };

        let (env, _, handle) = write_configs(&opts).await.unwrap();
        let prompt_path = std::path::PathBuf::from(env.get("GEMINI_SYSTEM_MD").unwrap());

        assert!(prompt_path.starts_with(artifact_dir.path()));
        drop(handle);
    }

    #[tokio::test]
    async fn generated_mcp_config_uses_owned_artifacts_without_touching_cwd() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifact_dir.path().to_string_lossy().into_owned()),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            mcp_servers: Some(HashMap::from([(
                "test".into(),
                McpServer {
                    command: Some("test-server".into()),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };

        let (env, cwd_override, handle) = write_configs(&opts).await.unwrap();
        let settings_path =
            std::path::PathBuf::from(env.get("GEMINI_CLI_SYSTEM_SETTINGS_PATH").unwrap());

        assert!(settings_path.starts_with(artifact_dir.path()));
        assert_eq!(cwd_override, None);
        assert!(!cwd.path().join(".gemini/settings.json").exists());
        drop(handle);
    }
}
