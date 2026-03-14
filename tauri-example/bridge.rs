// Tauri desktop app integration with cli-agents.
//
// This file depends on `tauri`, `specta`, and `log` which are not
// dependencies of this crate. Copy and adapt it into your own Tauri project.
// See README.md in this directory for usage details.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use tauri::Emitter;

/// Return type for the Tauri command — adapt to your app's needs.
pub struct ClaudeCliOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// State for the running CLI agent — holds the cancellation token.
pub struct CliBridgeState {
    cancel: Arc<Mutex<Option<CancellationToken>>>,
}

impl CliBridgeState {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(Mutex::new(None)),
        }
    }
}

/// Run an AI CLI agent using cli-agents.
///
/// The `config` parameter is a JSON string matching cli-agents RunOptions:
/// ```json
/// {
///   "cli": "claude",
///   "task": "...",
///   "systemPrompt": "...",
///   "model": "sonnet",
///   "skipPermissions": true,
///   "mcpServers": { ... },
///   "providers": { "claude": { "maxTurns": 20 } }
/// }
/// ```
///
/// Streams events via Tauri events:
/// - `cli-agent-stdout`: Each StreamEvent as JSON line
/// - `cli-agent-stderr`: Stderr output (if any)
/// - `cli-agent-done`: Final status `{ "code": 0, "stderr": "..." }`
#[tauri::command]
#[specta::specta]
pub async fn run_cli_agent(
    app: tauri::AppHandle,
    state: tauri::State<'_, CliBridgeState>,
    config: String,
    env: HashMap<String, String>,
) -> Result<ClaudeCliOutput, String> {
    // Parse RunOptions from config JSON
    let mut opts: cli_agents::RunOptions = serde_json::from_str(&config)
        .map_err(|e| format!("Invalid cli-agent config: {e}"))?;

    // Merge extra env
    if !env.is_empty() {
        let existing = opts.env.get_or_insert_with(HashMap::new);
        existing.extend(env);
    }

    log::info!(
        "[cli-bridge] Running cli-agents: cli={:?}, task_len={}",
        opts.cli,
        opts.task.len()
    );

    // Set up cancellation
    let cancel = CancellationToken::new();
    {
        let mut guard = state.cancel.lock().await;
        *guard = Some(cancel.clone());
    }

    // Event emitter — forward StreamEvents as JSON lines via Tauri events
    let app_clone = app.clone();
    let on_event: Arc<dyn Fn(cli_agents::StreamEvent) + Send + Sync> = Arc::new(move |event| {
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = app_clone.emit("cli-agent-stdout", &json);
        }
    });

    // Run the agent
    let handle = cli_agents::run(opts, Some(on_event));

    // Wait for completion or cancellation
    let result = tokio::select! {
        res = handle.result => {
            match res {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    return Err(format!("cli-agents error: {e}"));
                }
                Err(e) => {
                    return Err(format!("cli-agents task panicked: {e}"));
                }
            }
        }
        _ = cancel.cancelled() => {
            handle.abort();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                handle.result,
            ).await;

            cli_agents::RunResult {
                success: false,
                text: Some("Cancelled.".into()),
                exit_code: Some(-1),
                ..Default::default()
            }
        }
    };

    // Clear cancel token
    {
        let mut guard = state.cancel.lock().await;
        *guard = None;
    }

    let code = result.exit_code.unwrap_or(if result.success { 0 } else { 1 });
    let stderr_str = result.stderr.unwrap_or_default();

    let _ = app.emit(
        "cli-agent-done",
        serde_json::json!({ "code": code, "stderr": &stderr_str }),
    );

    Ok(ClaudeCliOutput {
        code,
        stdout: result.text.unwrap_or_default(),
        stderr: stderr_str,
    })
}

/// Cancel the running CLI agent.
#[tauri::command]
#[specta::specta]
pub async fn cancel_cli_agent(
    state: tauri::State<'_, CliBridgeState>,
) -> Result<(), String> {
    let guard = state.cancel.lock().await;
    if let Some(cancel) = guard.as_ref() {
        cancel.cancel();
        log::info!("[cli-bridge] Cancellation requested");
    }
    Ok(())
}
