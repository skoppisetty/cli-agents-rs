use clap::Parser;
use cli_agents::{CliName, RunOptions, StreamEvent, run};
use std::io::{BufRead, Read, Write};
use std::sync::Arc;

/// Unified AI CLI framework for Claude, Codex, and Gemini.
#[derive(Parser)]
#[command(name = "cli-agents", arg_required_else_help = true)]
struct Args {
    /// Read the complete library RunOptions JSON object from stdin.
    /// Runtime fields are used unchanged; prompts and credentials stay out of argv.
    #[arg(long, conflicts_with_all = ["cli", "model", "system", "append_system_prompt", "cwd", "skip_permissions", "discover", "task"])]
    config_stdin: bool,

    /// Read one opt-in Codex prepare/run/inspect request from stdin; never retries a turn.
    #[arg(long, conflicts_with_all = ["config_stdin", "cli", "model", "system", "append_system_prompt", "cwd", "skip_permissions", "discover", "task"])]
    codex_app_server_stdin: bool,

    /// Which CLI to use (auto-discovers if omitted)
    #[arg(long, value_parser = parse_cli_name)]
    cli: Option<CliName>,

    /// Model name (e.g. sonnet, opus, o3)
    #[arg(long)]
    model: Option<String>,

    /// System prompt
    #[arg(long)]
    system: Option<String>,

    /// Append to the system prompt
    #[arg(long)]
    append_system_prompt: Option<String>,

    /// Working directory
    #[arg(long)]
    cwd: Option<String>,

    /// Run without permission prompts
    #[arg(long)]
    skip_permissions: bool,

    /// Print all events as JSON lines
    #[arg(long)]
    json: bool,

    /// Print tool calls and thinking
    #[arg(long, short)]
    verbose: bool,

    /// List available CLIs and exit
    #[arg(long)]
    discover: bool,

    /// The task/prompt to send
    task: Vec<String>,
}

fn parse_cli_name(s: &str) -> Result<CliName, String> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|_| "unsupported CLI provider".to_owned())
}

/// Decode the exact shared library contract, without reinterpreting provider options.
fn run_options(args: &Args, input: impl Read) -> Result<RunOptions, &'static str> {
    if args.config_stdin {
        let opts: RunOptions = serde_json::from_reader(input)
            .map_err(|_| "stdin must contain one valid RunOptions JSON object")?;
        if opts.task.trim().is_empty() {
            return Err("no task provided");
        }
        return Ok(opts);
    }
    let task = args.task.join(" ");
    if task.is_empty() {
        return Err("no task provided. Run with --help for usage");
    }
    let system_prompt = match (&args.system, &args.append_system_prompt) {
        (Some(base), Some(extra)) => Some(format!("{base}\n\n{extra}")),
        (Some(base), None) => Some(base.clone()),
        (None, Some(extra)) => Some(extra.clone()),
        (None, None) => None,
    };
    Ok(RunOptions {
        cli: args.cli,
        task,
        system_prompt,
        cwd: args.cwd.clone(),
        model: args.model.clone(),
        skip_permissions: args.skip_permissions,
        providers: Some(cli_agents::ProviderOptions {
            claude: Some(cli_agents::ClaudeOptions {
                include_partial_messages: Some(true),
                append_system_prompt: args.append_system_prompt.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.codex_app_server_stdin {
        // One JSON line per request; Prepare emits a thread ID then awaits a second,
        // explicitly admitted {binding,task} line on the SAME provider connection.
        let mut line = String::new();
        if std::io::stdin().lock().take(1_048_577).read_line(&mut line).is_err() || line.len() > 1_048_576 {
            eprintln!("Invalid Codex app-server request"); std::process::exit(2);
        }
        let request: cli_agents::CodexAppServerRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => { eprintln!("Invalid Codex app-server request"); std::process::exit(2); }
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        // Embedders terminate this CLI with SIGTERM. Reap the separate provider
        // process group and retain uncertainty instead of orphaning its turn.
        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
                signal_cancel.cancel();
                return;
            }
            let _ = tokio::signal::ctrl_c().await;
            signal_cancel.cancel();
        });
        let json_progress = args.json;
        let progress: cli_agents::CodexProgress = Arc::new(move |event| {
            if json_progress { println!("{}", serde_json::to_string(&event).unwrap()); }
        });
        let result = match request {
            cli_agents::CodexAppServerRequest::Prepare { options } => {
                cli_agents::prepare_codex_turn_with_progress(options, Box::new(|thread_id| Box::pin(async move {
                    println!("{}", serde_json::json!({"state":"prepared","threadId":thread_id}));
                    std::io::stdout().flush().map_err(cli_agents::Error::Io)?;
                    tokio::task::spawn_blocking(|| {
                        let mut ack = String::new();
                        std::io::stdin().lock().take(1_048_577).read_line(&mut ack).map_err(cli_agents::Error::Io)?;
                        if ack.len() > 1_048_576 { return Err(cli_agents::Error::Other("Admission bound exceeded".into())); }
                        serde_json::from_str(&ack).map_err(|_| cli_agents::Error::Other("Invalid admission acknowledgement".into()))
                    }).await.map_err(|_| cli_agents::Error::Other("Admission interrupted".into()))?
                })), cancel, progress.clone()).await
            },
            other => cli_agents::codex_app_server_with_progress(other, cancel, progress).await,
        };
        match result {
            Ok(result) => println!("{}", serde_json::to_string(&result).unwrap()),
            Err(_) => { eprintln!("Codex app-server operation rejected"); std::process::exit(2); }
        }
        // A blocked stdin reader must not keep this bounded CLI alive after timeout.
        std::process::exit(0);
    }

    // --discover mode: list available CLIs
    if args.discover {
        let results = cli_agents::discovery::discover_all().await;
        if results.is_empty() {
            eprintln!("No CLI agents found on this system.");
            eprintln!("Install one of: claude, codex, gemini");
            std::process::exit(1);
        }
        for (name, path) in &results {
            println!("{name}: {path}");
        }
        std::process::exit(0);
    }

    let opts = match run_options(&args, std::io::stdin().lock()) {
        Ok(opts) => opts,
        Err(message) => {
            // Never format input JSON or serde errors: either can contain secrets.
            eprintln!("Error: {message}");
            std::process::exit(2);
        }
    };

    let json_mode = args.json;
    let verbose = args.verbose;

    let on_event: Arc<dyn Fn(StreamEvent) + Send + Sync> = if json_mode {
        // JSON lines mode — print every event as JSON to stdout
        Arc::new(move |event: StreamEvent| {
            if let Ok(json) = serde_json::to_string(&event) {
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "{json}");
            }
        })
    } else {
        // Human-readable mode — stream text, show tools/thinking if verbose
        let had_deltas = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Arc::new(move |event: StreamEvent| {
            match event {
                StreamEvent::TextDelta { text } => {
                    had_deltas.store(true, std::sync::atomic::Ordering::Relaxed);
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                StreamEvent::ThinkingDelta { text } if verbose => {
                    eprint!("\x1b[2m{text}\x1b[0m"); // dim
                    let _ = std::io::stderr().flush();
                }
                StreamEvent::ToolStart { tool_name, .. } if verbose => {
                    eprintln!("\x1b[36m▶ {tool_name}\x1b[0m");
                }
                StreamEvent::ToolEnd { success, error, .. } if verbose => {
                    if success {
                        eprintln!("\x1b[32m✓\x1b[0m");
                    } else {
                        eprintln!("\x1b[31m✗ {}\x1b[0m", error.unwrap_or_default());
                    }
                }
                StreamEvent::TurnEnd if verbose => {
                    eprintln!("\x1b[2m--- turn end ---\x1b[0m");
                }
                StreamEvent::Error { message, .. } => {
                    eprintln!("\x1b[31mError: {message}\x1b[0m");
                }
                StreamEvent::Done { result } => {
                    // Print final text if no deltas were streamed (e.g. -p mode)
                    if !had_deltas.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Some(text) = &result.text {
                            if !text.is_empty() {
                                print!("{text}");
                            }
                        }
                    }
                    println!();
                    if verbose {
                        if let Some(stats) = &result.stats {
                            eprint!("\x1b[2m");
                            if let Some(t) = stats.input_tokens {
                                eprint!("in={t} ");
                            }
                            if let Some(t) = stats.output_tokens {
                                eprint!("out={t} ");
                            }
                            if let Some(ms) = stats.duration_ms {
                                eprint!("time={ms}ms ");
                            }
                            if let Some(n) = stats.tool_calls {
                                eprint!("tools={n} ");
                            }
                            eprintln!("\x1b[0m");
                        }
                        if let Some(cost) = result.cost_usd {
                            eprintln!("\x1b[2mcost=${cost:.4}\x1b[0m");
                        }
                    }
                }
                _ => {}
            }
        })
    };

    let handle = run(opts, Some(on_event));

    // Only handle Ctrl+C when running as a top-level CLI (stdin is a terminal).
    // When spawned as a subprocess, let the parent handle SIGINT — otherwise
    // tokio::signal::ctrl_c() steals SIGINT from the parent's process group.
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let cancel = handle.cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            eprintln!("\nCancelling...");
            cancel.cancel();
        });
    }

    match handle.result.await {
        Ok(Ok(result)) => {
            let code = result
                .exit_code
                .unwrap_or(if result.success { 0 } else { 1 });
            std::process::exit(code);
        }
        Ok(Err(e)) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Panic: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stdin_preserves_the_complete_library_contract() {
        let args = Args::try_parse_from(["cli-agents", "--config-stdin", "--json"]).unwrap();
        let value = json!({
            "cli": "claude", "task": "Inspect and edit", "systemPrompt": "instructions",
            "model": "sonnet", "cwd": "/workspace", "resumeSessionId": "prior-session",
            "env": {"EXAMPLE_CREDENTIAL": "private-value"}, "clearEnv": true,
            "artifactDir": "/workspace/tmp", "idleTimeoutMs": 12000,
            "totalTimeoutMs": 30000, "maxConsecutiveToolFailures": 7,
            "maxOutputBytes": 4096, "skipPermissions": true,
            "mcpServers": {"cueframe": {"url": "http://localhost:1234/mcp", "headers": {"authorization": "Bearer secret"}}},
            "providers": {"claude": {"allowedTools": "mcp__cueframe__*", "agents": {"reviewer": {"description": "Review", "prompt": "Check edits"}}}}
        });
        let json = value.to_string();
        let expected: RunOptions = serde_json::from_str(&json).unwrap();
        let actual = run_options(&args, json.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn stdin_rejects_ambiguous_flag_and_positional_overrides() {
        for flags in [
            vec!["--cli", "claude"],
            vec!["--model", "sonnet"],
            vec!["--system", "override"],
            vec!["--append-system-prompt", "extra"],
            vec!["--cwd", "/tmp"],
            vec!["--skip-permissions"],
            vec!["--discover"],
            vec!["task"],
        ] {
            let mut argv = vec!["cli-agents", "--config-stdin"];
            argv.extend(flags);
            assert!(Args::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn invalid_input_diagnostics_never_echo_secrets() {
        let args = Args::try_parse_from(["cli-agents", "--config-stdin"]).unwrap();
        for input in [
            "",
            "{",
            r#"{"task":"x","cli":"secret-token"}"#,
            r#"{"task":""}"#,
            r#"{"task":"valid"} {"secret":"extra"}"#,
        ] {
            let error = run_options(&args, input.as_bytes()).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn simple_mode_keeps_existing_prompt_behavior() {
        let args = Args::try_parse_from([
            "cli-agents",
            "--cli",
            "codex",
            "--system",
            "base",
            "--append-system-prompt",
            "extra",
            "--",
            "--literal-task",
        ])
        .unwrap();
        let options = run_options(&args, std::io::empty()).unwrap();
        assert_eq!(options.task, "--literal-task");
        assert_eq!(options.system_prompt.as_deref(), Some("base\n\nextra"));
        assert_eq!(options.cli, Some(CliName::Codex));
    }
}
