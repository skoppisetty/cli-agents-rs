mod emit;

use crate::adapters::get_adapter;
use crate::discovery::discover_first;
use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{RunOptions, RunResult};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use emit::EmitWrapper;

const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000; // 5 minutes
const DEFAULT_MAX_CONSECUTIVE_TOOL_FAILURES: u32 = 3;

/// Handle returned by [`run()`] — allows awaiting the result or aborting.
#[must_use = "the run will be abandoned if the handle is dropped"]
pub struct RunHandle {
    pub result: tokio::task::JoinHandle<Result<RunResult>>,
    pub cancel: CancellationToken,
}

impl RunHandle {
    /// Abort the running agent.
    pub fn abort(&self) {
        self.cancel.cancel();
    }
}

/// Launch an agent run. Returns a [`RunHandle`] with the result future and abort handle.
pub fn run(
    opts: RunOptions,
    on_event: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
) -> RunHandle {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let result = tokio::spawn(async move { run_internal(opts, on_event, cancel_clone).await });

    RunHandle { result, cancel }
}

async fn run_internal(
    opts: RunOptions,
    on_event: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
    cancel: CancellationToken,
) -> Result<RunResult> {
    // Resolve adapter
    let cli_name = match opts.cli {
        Some(cli) => cli,
        None => {
            if opts.executable_path.is_some() {
                return Err(Error::CliRequiredWithExecutable);
            }
            let (name, _path) = discover_first().await.ok_or(Error::NoCli)?;
            name
        }
    };

    let adapter = get_adapter(cli_name);

    // Wrap emit with tool failure tracking + timeouts
    let wrapper = EmitWrapper::new(
        on_event.clone(),
        opts.idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
        opts.total_timeout_ms,
        opts.max_consecutive_tool_failures
            .unwrap_or(DEFAULT_MAX_CONSECUTIVE_TOOL_FAILURES),
        cancel.clone(),
    );

    let emit_fn = wrapper.make_emit_fn();

    let result = adapter.run_boxed(&opts, &emit_fn, cancel.clone()).await;

    wrapper.cleanup();

    match result {
        Ok(mut result) => {
            if cancel.is_cancelled() {
                result.success = false;
                result.text = Some("Cancelled.".into());
            }
            if let Some(ref cb) = on_event {
                cb(StreamEvent::Done {
                    result: result.clone(),
                });
            }
            Ok(result)
        }
        Err(e) => {
            if cancel.is_cancelled() {
                let result = RunResult {
                    success: false,
                    text: Some("Cancelled.".into()),
                    ..Default::default()
                };
                if let Some(ref cb) = on_event {
                    cb(StreamEvent::Done {
                        result: result.clone(),
                    });
                }
                Ok(result)
            } else {
                Err(e)
            }
        }
    }
}
