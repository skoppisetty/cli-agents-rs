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
    /// Process reached a terminal state on its own (exited, or was signalled).
    Done {
        /// The process's exit status, or `None` when a SIGNAL ended it instead.
        ///
        /// A signalled process genuinely has no exit code. Substituting one
        /// makes an out-of-memory kill indistinguishable from an agent that
        /// cleanly decided it had failed, and [`RunResult::exit_code`] is an
        /// `Option` precisely so a caller can tell the two apart.
        exit_code: Option<i32>,
        /// The signal that terminated the process, when one did. Unix only;
        /// always `None` elsewhere.
        signal: Option<i32>,
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
    /// Keys to remove from the inherited parent env before applying `extra_env`.
    /// Used to prevent leaks like `ANTHROPIC_API_KEY` overriding subscription auth.
    pub strip_env: &'a [&'static str],
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
        strip_env,
        cwd,
        max_bytes,
        cancel,
    } = params;
    debug!(cli = cli_label, binary = %binary, args = ?args, "spawning CLI");

    let mut cmd = Command::new(binary);
    cmd.args(args);
    for key in strip_env {
        cmd.env_remove(key);
    }
    cmd.envs(extra_env)
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
    // `code()` is `None` for a signalled process. This used to be
    // `.unwrap_or(1)`, which reported a SIGKILL as a clean `exit 1` and left
    // callers with no way to recover the difference — the exact ambiguity that
    // sent a downstream app hunting for an error message a killed process never
    // wrote. Report what actually happened and let the caller decide.
    let exit_code = status.code();
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let stderr_text = stderr_handle.await.unwrap_or_default();

    Ok(SpawnOutcome::Done {
        exit_code,
        signal,
        stderr: if stderr_text.is_empty() {
            None
        } else {
            Some(stderr_text)
        },
    })
}

/// A sentence for a process that died without writing one.
///
/// A signalled CLI usually produces NO stderr and no result event — there was
/// no chance to. Without this the only fact reaching the user is a number, and
/// the most common case by far (the OS reclaiming memory) reads as an
/// unexplained failure.
pub(crate) fn describe_signal(signal: Option<i32>) -> Option<String> {
    let sig = signal?;
    Some(match sig {
        2 => "The agent was interrupted (SIGINT).".to_string(),
        6 => "The agent aborted (SIGABRT).".to_string(),
        9 => "The agent was killed (SIGKILL), most often by the system reclaiming memory."
            .to_string(),
        11 => "The agent crashed (SIGSEGV).".to_string(),
        15 => "The agent was terminated (SIGTERM).".to_string(),
        other => format!("The agent was terminated by signal {other}."),
    })
}

/// Extract a user-friendly error message from CLI stderr.
/// When an agent fails with no text output, this provides something
/// meaningful to show the user instead of a blank response.
pub(crate) fn extract_error_message(stderr: Option<&str>) -> Option<String> {
    let stderr = stderr?;
    // Find the most informative error line.
    let msg = stderr
        .lines()
        .filter(|l| !l.is_empty())
        .find(|l| {
            let lower = l.to_lowercase();
            lower.contains("error")
                || lower.contains("limit")
                || lower.contains("failed")
                || lower.contains("denied")
                || lower.contains("unauthorized")
        })
        .or_else(|| stderr.lines().rfind(|l| !l.is_empty()));
    msg.map(|s| s.trim().to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that ends by SIGNAL has no exit code, and says so.
    ///
    /// THE REGRESSION THIS PINS. `spawn_and_stream` used to finish with
    /// `status.code().unwrap_or(1)`, so a killed process was reported as a
    /// clean `exit 1`. Downstream that is unrecoverable: an out-of-memory kill
    /// and an agent that decided it had failed become the same fact, and a
    /// consumer looking for the reason searches a stderr the process never got
    /// to write. `sh -c 'kill -9 $$'` reproduces it without timing games — the
    /// shell signals itself, so the outcome is deterministic.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signalled_process_reports_the_signal_not_a_fabricated_exit_code() {
        let args = vec!["-c".to_string(), "kill -9 $$".to_string()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                strip_env: &[],
                cwd: ".",
                max_bytes: 1024,
                cancel: &cancel,
            },
            |_| {},
        )
        .await
        .expect("spawn should succeed");

        match outcome {
            SpawnOutcome::Done {
                exit_code, signal, ..
            } => {
                assert_eq!(exit_code, None, "a signalled process has no exit code");
                assert_eq!(signal, Some(9), "SIGKILL should be reported as itself");
            }
            SpawnOutcome::Cancelled => panic!("nothing cancelled this run"),
        }
    }

    /// An ordinary non-zero exit still reports its code — the change above must
    /// not turn every failure into `None`.
    #[tokio::test]
    async fn a_normal_exit_still_reports_its_code() {
        let args = vec!["-c".to_string(), "exit 3".to_string()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                strip_env: &[],
                cwd: ".",
                max_bytes: 1024,
                cancel: &cancel,
            },
            |_| {},
        )
        .await
        .expect("spawn should succeed");

        match outcome {
            SpawnOutcome::Done {
                exit_code, signal, ..
            } => {
                assert_eq!(exit_code, Some(3));
                assert_eq!(signal, None, "an ordinary exit was not signalled");
            }
            SpawnOutcome::Cancelled => panic!("nothing cancelled this run"),
        }
    }

    /// The user-facing sentence for the case that writes no stderr at all.
    #[test]
    fn describe_signal_names_the_common_kills() {
        assert!(describe_signal(Some(9)).unwrap().contains("SIGKILL"));
        assert!(describe_signal(Some(9)).unwrap().contains("memory"));
        assert!(describe_signal(Some(15)).unwrap().contains("SIGTERM"));
        assert!(describe_signal(Some(42)).unwrap().contains("42"));
        assert_eq!(describe_signal(None), None);
    }
}
