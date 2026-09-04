mod parse;

use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use tokio_util::sync::CancellationToken;

pub struct AntigravityAdapter;

impl CliAdapter for AntigravityAdapter {
    fn name(&self) -> CliName {
        CliName::Antigravity
    }

    async fn run(
        &self,
        opts: &RunOptions,
        emit: &(dyn Fn(StreamEvent) + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<RunResult> {
        validate_options(opts)?;
        let binary = match &opts.executable_path {
            Some(path) => path.clone(),
            None => discover_binary(CliName::Antigravity)
                .await
                .ok_or(Error::NoCli)?,
        };

        let args = build_args(opts);
        let extra_env = opts.env.clone().unwrap_or_default();
        let max_bytes = opts.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        let mut state = parse::ParseState::default();

        let outcome = crate::adapters::spawn_and_stream(
            crate::adapters::SpawnParams {
                cli_label: "antigravity",
                binary: &binary,
                args: &args,
                extra_env: &extra_env,
                clear_env: opts.clear_env,
                strip_env: &[],
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
            crate::adapters::SpawnOutcome::Done {
                exit_code,
                signal,
                stderr,
                dropped_lines,
            } => {
                crate::adapters::warn_dropped_lines(dropped_lines, max_bytes, emit);
                let success = state.success.unwrap_or(exit_code == Some(0));
                let text = if !success
                    && state
                        .result_text
                        .as_deref()
                        .is_none_or(|text| text.is_empty())
                {
                    state
                        .error
                        .or_else(|| crate::adapters::extract_error_message(stderr.as_deref()))
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

fn validate_options(opts: &RunOptions) -> Result<()> {
    if opts.system_prompt.is_some() || opts.system_prompt_file.is_some() {
        return Err(Error::Other(
            "Antigravity CLI does not support system prompts in headless mode".into(),
        ));
    }
    if opts
        .mcp_servers
        .as_ref()
        .is_some_and(|servers| !servers.is_empty())
    {
        return Err(Error::Other(
            "Antigravity CLI does not support per-run MCP configuration; configure .agents/mcp_config.json in the workspace".into(),
        ));
    }

    let extra_args = opts
        .providers
        .as_ref()
        .and_then(|providers| providers.antigravity.as_ref())
        .and_then(|options| options.extra_args.as_deref())
        .unwrap_or_default();
    if let Some(arg) = extra_args.iter().find(|arg| is_reserved_arg(arg)) {
        return Err(Error::Other(format!(
            "Antigravity extra_args cannot override the headless streaming protocol: {arg}"
        )));
    }

    Ok(())
}

fn is_reserved_arg(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    matches!(
        flag,
        "-p" | "--print" | "--prompt" | "--input-format" | "--output-format" | "--gemini_dir"
    )
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
        args.push("--conversation".into());
        args.push(session_id.clone());
    }

    if opts.skip_permissions {
        args.push("--dangerously-skip-permissions".into());
    }

    let antigravity = opts
        .providers
        .as_ref()
        .and_then(|providers| providers.antigravity.as_ref());
    if let Some(options) = antigravity {
        if let Some(effort) = &options.effort {
            args.push("--effort".into());
            args.push(effort.clone());
        }
        if let Some(agent) = &options.agent {
            args.push("--agent".into());
            args.push(agent.clone());
        }
        if options.sandbox == Some(true) {
            args.push("--sandbox".into());
        }
        if let Some(timeout) = &options.print_timeout {
            args.push("--print-timeout".into());
            args.push(timeout.clone());
        }
        if let Some(state_dir) = &options.state_dir {
            args.push("--gemini_dir".into());
            args.push(state_dir.clone());
        }
        // Caller-supplied flags come last so newly added Antigravity options
        // remain usable without waiting for a crate release.
        if let Some(extra) = &options.extra_args {
            args.extend(extra.iter().cloned());
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AntigravityOptions, McpServer, ProviderOptions};
    use std::collections::HashMap;

    #[test]
    fn build_args_minimal() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        assert_eq!(
            build_args(&opts),
            vec!["-p", "hello", "--output-format", "stream-json"]
        );
    }

    #[test]
    fn build_args_with_antigravity_options() {
        let opts = RunOptions {
            task: "do something".into(),
            model: Some("gemini-3.5-flash-medium".into()),
            resume_session_id: Some("conversation-1".into()),
            skip_permissions: true,
            providers: Some(crate::types::ProviderOptions {
                antigravity: Some(crate::types::AntigravityOptions {
                    effort: Some("high".into()),
                    agent: Some("reviewer".into()),
                    sandbox: Some(true),
                    print_timeout: Some("10m".into()),
                    state_dir: Some("/owned/provider-state".into()),
                    extra_args: Some(vec!["--json-schema".into(), "string".into()]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--conversation", "conversation-1"])
        );
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert!(args.windows(2).any(|pair| pair == ["--agent", "reviewer"]));
        assert!(args.contains(&"--sandbox".to_string()));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--print-timeout", "10m"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--gemini_dir", "/owned/provider-state"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--json-schema", "string"])
        );
    }

    #[test]
    fn per_run_mcp_is_rejected_without_touching_the_workspace() {
        let mut servers = HashMap::new();
        servers.insert(
            "remote".into(),
            McpServer {
                url: Some("https://example.com/mcp".into()),
                exclude_tools: Some(vec!["dangerous".into()]),
                ..Default::default()
            },
        );
        let opts = RunOptions {
            task: "hello".into(),
            mcp_servers: Some(servers),
            ..Default::default()
        };
        let error = validate_options(&opts).unwrap_err().to_string();
        assert!(error.contains("does not support per-run MCP configuration"));
        assert!(error.contains(".agents/mcp_config.json"));
    }

    #[tokio::test]
    async fn rejecting_per_run_mcp_preserves_existing_config() {
        let workspace = tempfile::tempdir().unwrap();
        let agents_dir = workspace.path().join(".agents");
        tokio::fs::create_dir(&agents_dir).await.unwrap();
        let config_path = agents_dir.join("mcp_config.json");
        let existing = r#"{"mcpServers":{"existing":{"command":"keep-me"}}}"#;
        tokio::fs::write(&config_path, existing).await.unwrap();

        let opts = RunOptions {
            task: "hello".into(),
            cwd: Some(workspace.path().to_string_lossy().into_owned()),
            executable_path: Some("/not/used/because-validation-runs-first".into()),
            mcp_servers: Some(HashMap::from([(
                "replacement".into(),
                McpServer {
                    command: Some("do-not-write".into()),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let error = AntigravityAdapter
            .run(&opts, &|_| {}, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("does not support per-run MCP configuration"));
        assert_eq!(
            tokio::fs::read_to_string(config_path).await.unwrap(),
            existing
        );
    }

    #[test]
    fn protocol_overrides_are_rejected() {
        for arg in [
            "-p",
            "--print",
            "--prompt=other",
            "--input-format",
            "--input-format=stream-json",
            "--output-format",
            "--output-format=json",
            "--gemini_dir",
            "--gemini_dir=/caller-state",
        ] {
            let opts = RunOptions {
                task: "hello".into(),
                providers: Some(ProviderOptions {
                    antigravity: Some(AntigravityOptions {
                        extra_args: Some(vec![arg.into()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let error = validate_options(&opts).unwrap_err().to_string();
            assert!(error.contains(arg), "missing rejected argument in: {error}");
        }
    }

    #[test]
    fn unrelated_extra_args_remain_supported() {
        let opts = RunOptions {
            task: "hello".into(),
            providers: Some(ProviderOptions {
                antigravity: Some(AntigravityOptions {
                    extra_args: Some(vec!["--json-schema".into(), "string".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        validate_options(&opts).unwrap();
        let args = build_args(&opts);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--json-schema", "string"])
        );
    }
}
