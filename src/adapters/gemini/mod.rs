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
        let (config_env, _tmp_dir) = write_configs(opts).await?;

        let cli_args = build_args(opts);
        let mut extra_env = opts.env.clone().unwrap_or_default();
        extra_env.extend(config_env);
        let max_bytes = opts.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

        let mut state = parse::ParseState::default();

        let outcome = crate::adapters::spawn_and_stream(
            crate::adapters::SpawnParams {
                cli_label: "gemini",
                binary: &binary,
                args: &cli_args,
                extra_env: &extra_env,
                cwd: opts.cwd.as_deref().unwrap_or("."),
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
            crate::adapters::SpawnOutcome::Done { exit_code, stderr } => Ok(RunResult {
                success: exit_code == 0,
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

    if let Some(session_id) = &opts.resume_session_id {
        args.push("--resume".into());
        args.push(session_id.clone());
    }

    // Permission bypass for non-interactive use (opt-in)
    if opts.skip_permissions {
        args.push("--yolo".into());
    }

    if let Some(gemini) = opts.providers.as_ref().and_then(|p| p.gemini.as_ref()) {
        if gemini.sandbox == Some(true) {
            args.push("-s".into());
        }
        if let Some(mode) = &gemini.approval_mode {
            args.push("--approval-mode".into());
            args.push(mode.clone());
        }
        if let Some(extra) = &gemini.extra_args {
            args.extend(extra.clone());
        }
    }

    args
}

/// Write temporary config files for MCP servers and system prompts.
///
/// Returns the env vars to set and the temp dir handle (must be kept alive
/// until the child process exits). Only allocates a temp dir when needed.
async fn write_configs(
    opts: &RunOptions,
) -> Result<(HashMap<String, String>, Option<tempfile::TempDir>)> {
    let has_mcp = opts.mcp_servers.as_ref().is_some_and(|s| !s.is_empty());
    let needs_prompt_file = opts.system_prompt_file.is_none() && opts.system_prompt.is_some();

    // system_prompt_file doesn't need a temp dir — it points to the file directly.
    if !has_mcp && !needs_prompt_file {
        let mut env = HashMap::new();
        if let Some(path) = &opts.system_prompt_file {
            env.insert("GEMINI_SYSTEM_MD".into(), path.clone());
        }
        return Ok((env, None));
    }

    let tmp_dir = tempfile::tempdir().map_err(Error::Io)?;
    let mut env = HashMap::new();

    // MCP servers → .gemini/settings.json
    if let Some(servers) = &opts.mcp_servers {
        if !servers.is_empty() {
            let gemini_dir = tmp_dir.path().join(".gemini");
            tokio::fs::create_dir_all(&gemini_dir)
                .await
                .map_err(Error::Io)?;

            let settings = build_mcp_settings(servers);
            let settings_path = gemini_dir.join("settings.json");
            tokio::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)
                .await
                .map_err(Error::Io)?;

            env.insert(
                "GEMINI_HOME".into(),
                tmp_dir.path().to_string_lossy().into_owned(),
            );
        }
    }

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

    Ok((env, Some(tmp_dir)))
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
}
