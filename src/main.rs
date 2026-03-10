use clap::Parser;
use cli_agents::{CliName, RunOptions, StreamEvent, run};
use std::io::Write;
use std::sync::Arc;

/// Unified AI CLI framework for Claude, Codex, and Gemini.
#[derive(Parser)]
#[command(name = "cli-agents", arg_required_else_help = true)]
struct Args {
    /// Which CLI to use (auto-discovers if omitted)
    #[arg(long, value_parser = parse_cli_name)]
    cli: Option<CliName>,

    /// Model name (e.g. sonnet, opus, o3)
    #[arg(long)]
    model: Option<String>,

    /// System prompt
    #[arg(long)]
    system: Option<String>,

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
    match s {
        "claude" => Ok(CliName::Claude),
        "codex" => Ok(CliName::Codex),
        "gemini" => Ok(CliName::Gemini),
        other => Err(format!(
            "unknown CLI '{other}': use claude, codex, or gemini"
        )),
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

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

    let task = args.task.join(" ");
    if task.is_empty() {
        eprintln!("Error: no task provided. Run with --help for usage.");
        std::process::exit(1);
    }

    let opts = RunOptions {
        cli: args.cli,
        task,
        system_prompt: args.system,
        cwd: args.cwd,
        model: args.model,
        skip_permissions: args.skip_permissions,
        ..Default::default()
    };

    let cli_label = args
        .cli
        .map(|c| c.to_string())
        .unwrap_or_else(|| "auto".into());
    eprintln!(
        "Running with cli={cli_label}, skip_permissions={}",
        args.skip_permissions
    );

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

    // Handle Ctrl+C — abort the run
    let cancel = handle.cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nCancelling...");
        cancel.cancel();
    });

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
