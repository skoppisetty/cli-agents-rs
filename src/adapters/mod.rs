mod claude;
mod codex;
mod gemini;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use gemini::GeminiAdapter;

use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

/// Trait implemented by each CLI adapter.
pub trait CliAdapter: Send + Sync {
    fn name(&self) -> CliName;

    fn run(
        &self,
        opts: &RunOptions,
        emit: &(dyn Fn(StreamEvent) + Send + Sync),
        cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = crate::error::Result<RunResult>> + Send;
}

/// Get the adapter for a given CLI.
pub(crate) fn get_adapter(cli: CliName) -> Box<dyn CliAdapterBoxed> {
    match cli {
        CliName::Claude => Box::new(ClaudeAdapter),
        CliName::Codex => Box::new(CodexAdapter),
        CliName::Gemini => Box::new(GeminiAdapter),
    }
}

/// Object-safe version of [`CliAdapter`] for dynamic dispatch.
///
/// Needed because `CliAdapter::run` uses RPITIT (`impl Future`), which makes
/// the trait non-object-safe. This wrapper boxes the future for `dyn` dispatch.
/// The blanket impl below bridges the two automatically.
#[allow(dead_code)]
pub(crate) trait CliAdapterBoxed: Send + Sync {
    fn name(&self) -> CliName;

    fn run_boxed<'a>(
        &'a self,
        opts: &'a RunOptions,
        emit: &'a (dyn Fn(StreamEvent) + Send + Sync),
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::error::Result<RunResult>> + Send + 'a>,
    >;
}

impl<T: CliAdapter> CliAdapterBoxed for T {
    fn name(&self) -> CliName {
        CliAdapter::name(self)
    }

    fn run_boxed<'a>(
        &'a self,
        opts: &'a RunOptions,
        emit: &'a (dyn Fn(StreamEvent) + Send + Sync),
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::error::Result<RunResult>> + Send + 'a>,
    > {
        Box::pin(self.run(opts, emit, cancel))
    }
}

// ── Shared subprocess infrastructure ──

/// Outcome of a spawned CLI process.
pub(crate) enum SpawnOutcome {
    /// Process exited normally.
    Done {
        exit_code: i32,
        stderr: Option<String>,
    },
    /// Process was cancelled via the cancellation token.
    Cancelled,
}

/// Parameters for [`spawn_and_stream`].
pub(crate) struct SpawnParams<'a> {
    pub cli_label: &'a str,
    pub binary: &'a str,
    pub args: &'a [String],
    pub extra_env: &'a HashMap<String, String>,
    pub cwd: &'a str,
    pub max_bytes: usize,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

/// Spawn a CLI subprocess and stream its stdout line-by-line.
///
/// Handles the boilerplate shared across all adapters: process spawning,
/// stdout buffering with size limits, stderr collection, and cancellation.
/// Does **not** clone the parent process environment — `Command` inherits it
/// automatically; only `extra_env` entries are added.
pub(crate) async fn spawn_and_stream(
    params: SpawnParams<'_>,
    mut on_line: impl FnMut(&str) + Send,
) -> Result<SpawnOutcome> {
    let SpawnParams {
        cli_label,
        binary,
        args,
        extra_env,
        cwd,
        max_bytes,
        cancel,
    } = params;
    debug!(cli = cli_label, binary = %binary, args = ?args, "spawning CLI");

    let mut cmd = Command::new(binary);
    cmd.args(args)
        .envs(extra_env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Process(format!("failed to spawn {cli_label}: {e}")))?;

    let child_pid = child.id();

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {}
        buf
    });

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut total_bytes: usize = 0;

    loop {
        line.clear();
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        total_bytes += n;
                        if total_bytes > max_bytes {
                            warn!(cli = cli_label, total_bytes, max_bytes, "output exceeded max buffer size");
                            kill_process_group(&mut child, child_pid).await;
                            return Err(Error::Process(format!(
                                "output exceeded max buffer size ({max_bytes} bytes)"
                            )));
                        }
                        on_line(line.trim());
                    }
                    Err(e) => {
                        warn!(cli = cli_label, error = %e, "error reading stdout");
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => {
                kill_process_group(&mut child, child_pid).await;
                return Ok(SpawnOutcome::Cancelled);
            }
        }
    }

    let status = child.wait().await.map_err(Error::Io)?;
    let exit_code = status.code().unwrap_or(1);
    let stderr_text = stderr_handle.await.unwrap_or_default();

    Ok(SpawnOutcome::Done {
        exit_code,
        stderr: if stderr_text.is_empty() {
            None
        } else {
            Some(stderr_text)
        },
    })
}

async fn kill_process_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill().await;
}
