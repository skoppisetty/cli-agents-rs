pub mod app_server;
mod parse;

use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::adapters::CliAdapter;
use crate::discovery::discover_binary;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
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

        // Keep only per-invocation configuration temporary. Session/auth paths
        // must keep their original CODEX_HOME for SQLite rollout references.
        let config = write_configs(opts).await?;
        let mut args = config.args.clone();
        args.extend(build_args(opts));
        let mut extra_env = opts.env.clone().unwrap_or_default();
        if let Some(home) = &config.home {
            extra_env.insert("CODEX_HOME".into(), home.to_string_lossy().into_owned());
        }
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
    // These are root CLI options, not `exec resume` options. Putting them after
    // `resume` rejects otherwise valid continuation requests in current Codex.
    let mut args = Vec::new();
    if let Some(cwd) = &opts.cwd {
        args.extend(["-C".into(), cwd.clone()]);
    }
    let codex_opts = opts.providers.as_ref().and_then(|p| p.codex.as_ref());
    if let Some(co) = codex_opts {
        if let Some(policy) = &co.approval_policy {
            match policy.as_str() {
                // Native replacement for the removed --full-auto shorthand.
                "full-auto" => {
                    args.extend(["--ask-for-approval".into(), "on-request".into()]);
                    if co.sandbox_mode.is_none() {
                        args.extend(["--sandbox".into(), "workspace-write".into()]);
                    }
                }
                "suggest" | "auto-edit" => {}
                other => warn!(policy = other, "unknown Codex approval policy, ignoring"),
            }
        }
        if let Some(sandbox) = &co.sandbox_mode {
            args.extend(["--sandbox".into(), sandbox.clone()]);
        }
        if let Some(dirs) = &co.additional_directories {
            for dir in dirs {
                args.extend(["--add-dir".into(), dir.clone()]);
            }
        }
    }
    args.push("exec".into());
    if let Some(session_id) = &opts.resume_session_id {
        args.extend(["resume".into(), session_id.clone()]);
    }
    args.push("--json".into());
    if let Some(model) = &opts.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(co) = codex_opts {
        if let Some(images) = &co.images {
            for img in images {
                args.extend(["--image".into(), img.clone()]);
            }
        }
        if let Some(schema) = &co.output_schema {
            args.extend(["--output-schema".into(), schema.clone()]);
        }
    }
    let has_policy = codex_opts
        .and_then(|c| c.approval_policy.as_deref())
        .is_some_and(|p| !p.is_empty());
    if opts.skip_permissions && !has_policy {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    }
    if opts.skip_permissions {
        args.push("--skip-git-repo-check".into());
    }
    if let Some(extra) = codex_opts.and_then(|c| c.extra_args.as_ref()) {
        args.extend(extra.iter().cloned());
    }
    // Positional prompt must not become an option when the user starts with '-'.
    args.extend(["--".into(), opts.task.clone()]);
    args
}

// Codex's named profile files preserve the durable home and keep secrets off argv.
#[derive(Default, Serialize)]
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
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disabled_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_timeout_sec: Option<u64>,
}

#[derive(Default)]
struct InvocationConfig {
    home: Option<PathBuf>,
    args: Vec<String>,
    profile: Option<tempfile::NamedTempFile>,
    artifacts: Option<tempfile::TempDir>,
}

async fn write_configs(opts: &RunOptions) -> Result<InvocationConfig> {
    let system_prompt = resolve_system_prompt(opts).await?;
    if opts.mcp_servers.is_none() && system_prompt.is_none() {
        return Ok(InvocationConfig::default());
    }
    let home = resolve_codex_home(opts).ok_or_else(|| {
        Error::Other("Codex configuration requires a persistent child CODEX_HOME or HOME".into())
    })?;
    std::fs::create_dir_all(&home).map_err(Error::Io)?;
    let home = std::fs::canonicalize(home).map_err(Error::Io)?;
    let runtime_home = prepare_runtime_home(&home)?;
    let base_path = home.join("config.toml");
    let mut table: toml::Table = if base_path.exists() {
        toml::from_str(&std::fs::read_to_string(&base_path).map_err(Error::Io)?)
            .map_err(|_| Error::Other("Invalid existing Codex configuration".into()))?
    } else {
        toml::Table::new()
    };
    let mut config = InvocationConfig {
        home: Some(runtime_home.clone()),
        ..Default::default()
    };
    if let Some(prompt) = system_prompt {
        let dir = crate::artifacts::temp_dir(opts, "cli-agents-codex-")?;
        let path = dir.path().join("instructions.md");
        tokio::fs::write(&path, prompt).await.map_err(Error::Io)?;
        table.insert(
            "model_instructions_file".into(),
            toml::Value::String(path.to_string_lossy().into_owned()),
        );
        config.artifacts = Some(dir);
    }
    if let Some(servers) = &opts.mcp_servers {
        let mut mcp = toml::Table::new();
        for (name, server) in servers {
            if matches!(server.transport_type, Some(crate::types::McpTransport::Sse)) {
                return Err(Error::Other(
                    "Codex requires streamable HTTP or stdio MCP transport".into(),
                ));
            }
            let mapped = CodexMcpServer {
                command: server.command.clone(),
                args: server.args.clone(),
                env: server.env.clone(),
                cwd: server.cwd.clone(),
                url: server.url.clone(),
                http_headers: server.headers.clone(),
                enabled_tools: server.include_tools.clone(),
                disabled_tools: server.exclude_tools.clone(),
                tool_timeout_sec: server.timeout,
            };
            mcp.insert(
                name.clone(),
                toml::Value::try_from(mapped)
                    .map_err(|_| Error::Other("Cannot serialize Codex MCP configuration".into()))?,
            );
        }
        table.insert("mcp_servers".into(), toml::Value::Table(mcp));
    }
    let mut profile = tempfile::Builder::new()
        .prefix("cli-agents-")
        .suffix(".config.toml")
        .tempfile_in(&runtime_home)
        .map_err(Error::Io)?;
    let content = toml::to_string(&table)
        .map_err(|_| Error::Other("Cannot serialize Codex invocation configuration".into()))?;
    profile.write_all(content.as_bytes()).map_err(Error::Io)?;
    let filename = profile
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Other("Invalid Codex profile filename".into()))?;
    let name = filename.strip_suffix(".config.toml").unwrap();
    config.args = vec!["--profile".into(), name.into()];
    config.profile = Some(profile);
    Ok(config)
}

/// Stable paths are required: Codex records absolute rollout paths in SQLite.
/// Only invocation profiles are temporary; owner transcripts/auth stay in place.
fn prepare_runtime_home(owner: &Path) -> Result<PathBuf> {
    const RUNTIME: &str = ".cli-agents-runtime";
    for name in ["sessions", "archived_sessions"] {
        std::fs::create_dir_all(owner.join(name)).map_err(Error::Io)?;
    }
    let runtime = owner.join(RUNTIME);
    std::fs::create_dir_all(&runtime).map_err(Error::Io)?;
    for entry in std::fs::read_dir(owner).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name();
        // The runtime owns no base config. Every profile is a complete snapshot
        // of caller config, so profile merging cannot restore removed MCP fields.
        if name == "config.toml"
            || name == RUNTIME
            || name.to_string_lossy().ends_with(".config.toml")
        {
            continue;
        }
        let target = runtime.join(name);
        if std::fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(entry.path(), &target);
        #[cfg(windows)]
        let result = if entry.path().is_dir() {
            std::os::windows::fs::symlink_dir(entry.path(), &target)
        } else {
            std::os::windows::fs::symlink_file(entry.path(), &target)
        };
        if let Err(error) = result {
            // Concurrent invocations may prepare the same owner home.
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(Error::Io(error));
            }
        }
    }
    Ok(runtime)
}

fn resolve_codex_home(opts: &RunOptions) -> Option<PathBuf> {
    let configured = opts.env.as_ref().and_then(|env| {
        env.get("CODEX_HOME").map(PathBuf::from).or_else(|| {
            env.get("HOME")
                .map(|home| PathBuf::from(home).join(".codex"))
        })
    });
    if configured.is_some() || opts.artifact_dir.is_some() || opts.clear_env {
        return configured;
    }
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
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
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ask-for-approval", "on-request"])
        );
        assert!(!args.contains(&"--full-auto".to_string()));
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
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ask-for-approval", "on-request"])
        );
        assert!(!args.contains(&"--full-auto".to_string()));
        assert!(
            !args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()),
            "should not pass both --full-auto and --dangerously-bypass-approvals-and-sandbox"
        );
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
    }

    #[test]
    fn resume_keeps_root_scope_flags_before_exec_and_preserves_prompt() {
        let opts = RunOptions {
            task: "--literal prompt".into(),
            cwd: Some("/work".into()),
            resume_session_id: Some("session".into()),
            providers: Some(crate::types::ProviderOptions {
                codex: Some(crate::types::CodexOptions {
                    sandbox_mode: Some("workspace-write".into()),
                    additional_directories: Some(vec!["/assets".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let args = build_args(&opts);
        let exec = args.iter().position(|arg| arg == "exec").unwrap();
        assert_eq!(
            &args[..exec],
            [
                "-C",
                "/work",
                "--sandbox",
                "workspace-write",
                "--add-dir",
                "/assets"
            ]
        );
        assert_eq!(
            &args[exec..],
            [
                "exec",
                "resume",
                "session",
                "--json",
                "--",
                "--literal prompt"
            ]
        );
    }

    fn scoped_options(home: &tempfile::TempDir) -> RunOptions {
        RunOptions {
            task: "hello".into(),
            env: Some(HashMap::from([(
                "CODEX_HOME".into(),
                home.path().to_string_lossy().into_owned(),
            )])),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn profile_preserves_state_and_secret_transport_fields() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[mcp_servers.ambient]\ncommand = 'old'\n[mcp_servers.http]\ncommand = 'old-stdio'\n",
        )
        .unwrap();
        std::fs::create_dir(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("sessions/rollout.jsonl"), "saved").unwrap();
        let opts = RunOptions {
            mcp_servers: Some(HashMap::from([
                (
                    "stdio".into(),
                    crate::types::McpServer {
                        command: Some("tool".into()),
                        env: Some(HashMap::from([("SECRET".into(), "private-value".into())])),
                        ..Default::default()
                    },
                ),
                (
                    "http".into(),
                    crate::types::McpServer {
                        url: Some("https://mcp.test".into()),
                        transport_type: Some(crate::types::McpTransport::Http),
                        headers: Some(HashMap::from([(
                            "Authorization".into(),
                            "Bearer private-value".into(),
                        )])),
                        ..Default::default()
                    },
                ),
            ])),
            ..scoped_options(&home)
        };
        let config = write_configs(&opts).await.unwrap();
        let profile_path = config.profile.as_ref().unwrap().path().to_path_buf();
        let table: toml::Table =
            toml::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
        assert_eq!(
            table["mcp_servers"]["http"]["url"].as_str(),
            Some("https://mcp.test")
        );
        assert_eq!(
            table["mcp_servers"]["http"]["http_headers"]["Authorization"].as_str(),
            Some("Bearer private-value")
        );
        assert_eq!(
            table["mcp_servers"]["stdio"]["env"]["SECRET"].as_str(),
            Some("private-value")
        );
        assert!(
            table["mcp_servers"]
                .as_table()
                .unwrap()
                .get("ambient")
                .is_none()
        );
        assert!(
            table["mcp_servers"]["http"]
                .as_table()
                .unwrap()
                .get("command")
                .is_none()
        );
        assert!(!config.args.join(" ").contains("private-value"));
        assert_eq!(profile_path.parent(), config.home.as_deref());
        assert!(!config.home.as_ref().unwrap().join("config.toml").exists());
        assert_eq!(
            std::fs::canonicalize(config.home.as_ref().unwrap().join("sessions")).unwrap(),
            std::fs::canonicalize(home.path().join("sessions")).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&profile_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(config);
        assert!(!profile_path.exists());
        assert_eq!(
            std::fs::read_to_string(home.path().join("sessions/rollout.jsonl")).unwrap(),
            "saved"
        );
        assert!(home.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn concurrent_profiles_keep_one_stable_home_and_independent_configuration() {
        let home = tempfile::tempdir().unwrap();
        let first = RunOptions {
            system_prompt: Some("first".into()),
            ..scoped_options(&home)
        };
        let second = RunOptions {
            system_prompt: Some("second".into()),
            ..scoped_options(&home)
        };
        let (first, second) = tokio::join!(write_configs(&first), write_configs(&second));
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.home, second.home);
        assert_ne!(
            first.profile.as_ref().unwrap().path(),
            second.profile.as_ref().unwrap().path()
        );
        let second_path = second.profile.as_ref().unwrap().path().to_path_buf();
        drop(first);
        assert!(second_path.exists());
        assert!(home.path().join("sessions").is_dir());
        assert!(home.path().join("archived_sessions").is_dir());
        assert!(
            !second
                .home
                .as_ref()
                .unwrap()
                .join(".cli-agents-runtime")
                .exists()
        );
    }

    #[tokio::test]
    async fn instruction_file_uses_owned_artifacts_and_file_precedence() {
        let home = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let input = artifacts.path().join("input.md");
        std::fs::write(&input, "file instructions").unwrap();
        let opts = RunOptions {
            artifact_dir: Some(artifacts.path().to_string_lossy().into_owned()),
            system_prompt: Some("inline ignored".into()),
            system_prompt_file: Some(input.to_string_lossy().into_owned()),
            ..scoped_options(&home)
        };
        let config = write_configs(&opts).await.unwrap();
        let table: toml::Table = toml::from_str(
            &std::fs::read_to_string(config.profile.as_ref().unwrap().path()).unwrap(),
        )
        .unwrap();
        let path = PathBuf::from(table["model_instructions_file"].as_str().unwrap());
        assert!(path.starts_with(artifacts.path()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "file instructions");
        drop(config);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn empty_configuration_creates_no_profile() {
        let config = write_configs(&RunOptions::default()).await.unwrap();
        assert!(config.args.is_empty());
        assert!(config.profile.is_none());
    }

    #[test]
    fn owned_artifacts_never_import_ambient_home() {
        let opts = RunOptions {
            artifact_dir: Some("/isolated".into()),
            ..Default::default()
        };
        assert!(resolve_codex_home(&opts).is_none());
        let opts = RunOptions {
            clear_env: true,
            ..Default::default()
        };
        assert!(resolve_codex_home(&opts).is_none());
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
