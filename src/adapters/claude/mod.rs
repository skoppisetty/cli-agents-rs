mod parse;

use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use crate::DEFAULT_MAX_OUTPUT_BYTES;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

pub struct ClaudeAdapter;

impl CliAdapter for ClaudeAdapter {
    fn name(&self) -> CliName {
        CliName::Claude
    }

    async fn run(
        &self,
        opts: &RunOptions,
        emit: &(dyn Fn(StreamEvent) + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<RunResult> {
        let binary = match &opts.executable_path {
            Some(p) => p.clone(),
            None => discover_binary(CliName::Claude).await.ok_or(Error::NoCli)?,
        };

        let args = build_args(opts);
        let extra_env = opts.env.clone().unwrap_or_default();
        let max_bytes = opts.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

        let mut state = parse::ParseState::default();
        let mut active_tools: HashMap<String, String> = HashMap::new();

        let outcome = crate::adapters::spawn_and_stream(
            crate::adapters::SpawnParams {
                cli_label: "claude",
                binary: &binary,
                args: &args,
                extra_env: &extra_env,
                // Strip Anthropic API auth env vars so the CLI uses its own
                // subscription credentials (OAuth/keychain). Without this, a
                // user's shell `ANTHROPIC_API_KEY` would silently bill API
                // tokens instead of using the Claude Code subscription.
                strip_env: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
                cwd: opts.cwd.as_deref().unwrap_or("."),
                max_bytes,
                cancel: &cancel,
            },
            |line| parse::parse_line(line, &mut state, &mut active_tools, emit),
        )
        .await?;

        match outcome {
            crate::adapters::SpawnOutcome::Cancelled => Ok(RunResult {
                success: false,
                text: Some("Cancelled.".into()),
                ..Default::default()
            }),
            crate::adapters::SpawnOutcome::Done { exit_code, stderr } => {
                let success = state.success.unwrap_or(exit_code == 0);
                // When the agent fails with no text, surface the error from
                // stderr so consumers always have something to show the user.
                let text = if !success && state.result_text.is_none() {
                    crate::adapters::extract_error_message(stderr.as_deref())
                } else {
                    state.result_text
                };
                Ok(RunResult {
                    success,
                    text,
                    exit_code: Some(exit_code),
                    stats: state.stats,
                    session_id: state.session_id,
                    stderr,
                    cost_usd: state.cost_usd,
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
        "--verbose".into(),
    ];

    if let Some(model) = &opts.model {
        args.push("--model".into());
        args.push(model.clone());
    }

    if let Some(session_id) = &opts.resume_session_id {
        args.push("--resume".into());
        args.push(session_id.clone());
    }

    let claude_opts = opts.providers.as_ref().and_then(|p| p.claude.as_ref());

    if let Some(co) = claude_opts {
        if let Some(allowed) = &co.allowed_tools {
            args.push("--allowedTools".into());
            args.push(allowed.clone());
        }
        if let Some(disallowed) = &co.disallowed_tools {
            args.push("--disallowedTools".into());
            args.push(disallowed.clone());
        }
        if let Some(tools) = &co.tools {
            args.push("--tools".into());
            args.push(tools.clone());
        }
        if let Some(append) = &co.append_system_prompt {
            args.push("--append-system-prompt".into());
            args.push(append.clone());
        }
        if let Some(max_turns) = co.max_turns {
            args.push("--max-turns".into());
            args.push(max_turns.to_string());
        }
        if let Some(budget) = co.max_budget_usd {
            args.push("--max-budget-usd".into());
            args.push(budget.to_string());
        }
        if let Some(tokens) = co.max_thinking_tokens {
            args.push("--max-thinking-tokens".into());
            args.push(tokens.to_string());
        }
        if co.continue_session == Some(true) {
            args.push("--continue".into());
        }
        if co.include_partial_messages == Some(true) {
            args.push("--include-partial-messages".into());
        }
        if let Some(effort) = &co.effort {
            args.push("--effort".into());
            args.push(effort.clone());
        }
        if let Some(agents) = &co.agents {
            if let Ok(json) = serde_json::to_string(agents) {
                args.push("--agents".into());
                args.push(json);
            }
        }
        if let Some(sources) = &co.setting_sources {
            // `--setting-sources <comma-list>` — empty list loads nothing
            // (skips user/project/local settings and their SessionStart hooks).
            args.push("--setting-sources".into());
            args.push(
                sources
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }

    if let Some(path) = &opts.system_prompt_file {
        args.push("--system-prompt-file".into());
        args.push(path.clone());
    } else if let Some(system_prompt) = &opts.system_prompt {
        args.push("--system-prompt".into());
        args.push(system_prompt.clone());
    }

    // MCP servers: write inline JSON via --mcp-config (Claude CLI accepts this)
    if let Some(servers) = opts.mcp_servers.as_ref().filter(|s| !s.is_empty()) {
        if let Ok(json) = serde_json::to_string(&build_mcp_config(servers)) {
            args.push("--mcp-config".into());
            args.push(json);
        }
    }

    // Permission bypass for non-interactive use (opt-in)
    if opts.skip_permissions {
        args.push("--permission-mode".into());
        args.push("bypassPermissions".into());
        args.push("--dangerously-skip-permissions".into());
    }

    // User-supplied flags last, so callers can override earlier defaults
    // (e.g. force `--output-format text` or pass `--json-schema <schema>`).
    if let Some(extra) = claude_opts.and_then(|c| c.extra_args.as_ref()) {
        args.extend(extra.iter().cloned());
    }

    args
}

fn build_mcp_config(servers: &HashMap<String, crate::types::McpServer>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
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
            entry.insert("type".into(), serde_json::Value::String("stdio".into()));
            if let Some(cmd) = &server.command {
                entry.insert("command".into(), serde_json::Value::String(cmd.clone()));
            }
            if let Some(a) = &server.args {
                entry.insert("args".into(), serde_json::to_value(a).unwrap_or_default());
            }
            if let Some(e) = &server.env {
                entry.insert("env".into(), serde_json::to_value(e).unwrap_or_default());
            }
        }
        map.insert(name.clone(), serde_json::Value::Object(entry));
    }
    serde_json::Value::Object({
        let mut root = serde_json::Map::new();
        root.insert("mcpServers".into(), serde_json::Value::Object(map));
        root
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_claude_options() {
        let opts = RunOptions {
            task: "do stuff".into(),
            providers: Some(crate::types::ProviderOptions {
                claude: Some(crate::types::ClaudeOptions {
                    allowed_tools: Some("Bash,Read".into()),
                    disallowed_tools: Some("Write".into()),
                    tools: Some("Bash,Read,Write".into()),
                    max_turns: Some(10),
                    max_budget_usd: Some(1.5),
                    max_thinking_tokens: Some(8000),
                    continue_session: Some(true),
                    include_partial_messages: Some(true),
                    effort: Some("low".into()),
                    agents: Some(serde_json::json!({"reviewer": {"prompt": "review"}})),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(args.contains(&"Bash,Read".to_string()));
        assert!(args.contains(&"--disallowedTools".to_string()));
        assert!(args.contains(&"Write".to_string()));
        assert!(args.contains(&"--tools".to_string()));
        assert!(args.contains(&"Bash,Read,Write".to_string()));
        assert!(args.contains(&"--max-turns".to_string()));
        assert!(args.contains(&"10".to_string()));
        assert!(args.contains(&"--max-budget-usd".to_string()));
        assert!(args.contains(&"1.5".to_string()));
        assert!(args.contains(&"--max-thinking-tokens".to_string()));
        assert!(args.contains(&"8000".to_string()));
        assert!(args.contains(&"--continue".to_string()));
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--effort".to_string()));
        assert!(args.contains(&"low".to_string()));
        assert!(args.contains(&"--agents".to_string()));
    }

    #[test]
    fn build_args_system_prompt_file_takes_precedence() {
        let opts = RunOptions {
            task: "hello".into(),
            system_prompt: Some("inline prompt".into()),
            system_prompt_file: Some("/path/to/prompt.md".into()),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--system-prompt-file".to_string()));
        assert!(args.contains(&"/path/to/prompt.md".to_string()));
        assert!(!args.contains(&"--system-prompt".to_string()));
    }

    #[test]
    fn build_args_no_permission_bypass_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"bypassPermissions".to_string()));
    }

    #[test]
    fn build_args_permission_bypass_when_opted_in() {
        let opts = RunOptions {
            task: "hello".into(),
            skip_permissions: true,
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"bypassPermissions".to_string()));
    }

    #[test]
    fn build_args_setting_sources_omitted_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.contains(&"--setting-sources".to_string()));
    }

    #[test]
    fn build_args_setting_sources_empty_loads_none() {
        let opts = RunOptions {
            task: "hello".into(),
            providers: Some(crate::types::ProviderOptions {
                claude: Some(crate::types::ClaudeOptions {
                    setting_sources: Some(vec![]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        let idx = args
            .iter()
            .position(|a| a == "--setting-sources")
            .expect("flag emitted");
        assert_eq!(args[idx + 1], "");
    }

    #[test]
    fn build_args_setting_sources_subset() {
        use crate::types::SettingSource;
        let opts = RunOptions {
            task: "hello".into(),
            providers: Some(crate::types::ProviderOptions {
                claude: Some(crate::types::ClaudeOptions {
                    setting_sources: Some(vec![SettingSource::Project, SettingSource::Local]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        let idx = args
            .iter()
            .position(|a| a == "--setting-sources")
            .expect("flag emitted");
        assert_eq!(args[idx + 1], "project,local");
    }

    #[test]
    fn build_args_extra_args_omitted_by_default() {
        let opts = RunOptions {
            task: "hello".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        // Nothing about extra_args should appear when the option is unset.
        assert!(!args.iter().any(|a| a == "--json-schema"));
    }

    #[test]
    fn build_args_extra_args_appended_verbatim() {
        let opts = RunOptions {
            task: "hello".into(),
            providers: Some(crate::types::ProviderOptions {
                claude: Some(crate::types::ClaudeOptions {
                    extra_args: Some(vec!["--json-schema".into(), "{\"type\":\"object\"}".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        let idx = args
            .iter()
            .position(|a| a == "--json-schema")
            .expect("extra_args flag emitted");
        // Adjacent, in order — `args.extend` preserves both.
        assert_eq!(args[idx + 1], "{\"type\":\"object\"}");
    }

    #[test]
    fn build_args_extra_args_come_after_crate_defaults() {
        // Callers should be able to override what this crate emits — e.g.
        // re-pin `--output-format` to `text` even though we default to
        // stream-json. That only works if user-supplied flags land last.
        let opts = RunOptions {
            task: "hello".into(),
            skip_permissions: true,
            providers: Some(crate::types::ProviderOptions {
                claude: Some(crate::types::ClaudeOptions {
                    extra_args: Some(vec!["--output-format".into(), "text".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        // The crate emits `--output-format stream-json` early; the user
        // override must appear strictly after the permission-bypass block
        // (the last thing the crate emits).
        let user_idx = args
            .iter()
            .rposition(|a| a == "--output-format")
            .expect("user --output-format emitted");
        let bypass_idx = args
            .iter()
            .position(|a| a == "--dangerously-skip-permissions")
            .expect("skip_permissions flag emitted");
        assert!(
            user_idx > bypass_idx,
            "user extra_args must be appended last so they win over crate defaults"
        );
        assert_eq!(args[user_idx + 1], "text");
    }
}
