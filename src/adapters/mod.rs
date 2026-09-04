mod antigravity;
mod claude;
mod codex;

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;

use crate::error::{Error, Result};
use crate::events::StreamEvent;
use crate::types::{CliName, RunOptions, RunResult};
#[cfg(windows)]
use process_wrap::tokio::CreationFlags;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::TokioChildWrapper;
use process_wrap::tokio::TokioCommandWrap;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, warn};
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

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
        CliName::Antigravity => Box::new(AntigravityAdapter),
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
        /// How many stdout lines exceeded `max_bytes` and were dropped.
        ///
        /// A dropped line is a lost EVENT, not a lost run — adapters surface
        /// this as a warning so the loss is never silent.
        dropped_lines: u64,
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
    /// Start with no inherited parent-process environment.
    pub clear_env: bool,
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
/// `Command` inherits the parent environment unless `clear_env` is set;
/// `extra_env` entries are then overlaid in either mode.
pub(crate) async fn spawn_and_stream(
    params: SpawnParams<'_>,
    mut on_line: impl FnMut(&str) + Send,
) -> Result<SpawnOutcome> {
    let SpawnParams {
        cli_label,
        binary,
        args,
        extra_env,
        clear_env,
        strip_env,
        cwd,
        max_bytes,
        cancel,
    } = params;
    debug!(cli = cli_label, binary = %binary, args = ?args, "spawning CLI");

    // ── The child owns a KILL GROUP, on every platform ──
    //
    // Cancelling a run has to take the whole tree, not just the process we
    // spawned: `claude` is a launcher, and the work happens in node processes
    // below it. Killing only the parent orphans those — they keep running, keep
    // holding the model session, and keep writing to a pipe nobody reads.
    //
    // This used to be `pre_exec(setpgid)` plus `libc::killpg(SIGKILL)`, which is
    // correct on unix and does not exist on Windows — where the equivalent is a
    // Job Object, a completely different mechanism with the same purpose.
    // `process-wrap` is that difference, already written and tested: the unix
    // arm is the same process-group call, and the Windows arm assigns the child
    // to a job that dies with it.
    let mut wrap = TokioCommandWrap::with_new(binary, |cmd| {
        cmd.args(args);
        if clear_env {
            cmd.env_clear();
        }
        for key in strip_env {
            cmd.env_remove(key);
        }
        cmd.envs(extra_env)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // NOTE: CREATE_NO_WINDOW is NOT set here. See the wrap() calls below —
        // it MUST go through process-wrap's `CreationFlags` wrapper, or the
        // `JobObject` wrapper silently overwrites it.
    });

    // ── NO CONSOLE WINDOW, AND IT HAS TO GO THROUGH process-wrap ──
    //
    // Windows gives every console-subsystem child its own console window unless
    // the parent passes CREATE_NO_WINDOW at CreateProcess. `claude`, `codex` and
    // `agy` are console programs, so a GUI app embedding this crate flashes a
    // black terminal on every run — reported against a Tauri app, over the
    // user's editor.
    //
    // THE FIRST FIX (0.2.15) WAS SILENTLY CLOBBERED. It set
    // `cmd.creation_flags(CREATE_NO_WINDOW)` in the closure above. But the
    // `JobObject` wrapper's pre_spawn does `command.creation_flags(CREATE_SUSPENDED)`
    // (process-wrap 8.2.1, std/job_object.rs), and `creation_flags` REPLACES,
    // it does not OR — so the window flag was overwritten by the time the child
    // spawned. process-wrap reads CREATE_NO_WINDOW back ONLY from its own
    // `CreationFlags` wrapper (`core.get_wrap::<CreationFlags>()`), never from
    // the raw command, and ORs it into CREATE_SUSPENDED. Its docs say exactly
    // this: "the only way to use creation flags and the JobObject wrapper
    // together," and "CreationFlags must come first."
    //
    // So the flag is a WRAPPER, ordered before JobObject. Windows-only; on unix
    // ProcessGroup carries the tree-kill and there is no console to hide.
    #[cfg(windows)]
    wrap.wrap(CreationFlags(CREATE_NO_WINDOW));
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);

    let mut child = wrap
        .spawn()
        .map_err(|e| Error::Process(format!("failed to spawn {cli_label}: {e}")))?;

    let stdout = child.stdout().take().expect("stdout piped");
    let stderr = child.stderr().take().expect("stderr piped");

    // stderr is retained as a bounded TAIL. It exists so a failed run can be
    // explained (`extract_error_message` reads it), and the parting words are
    // at the end — a chatty CLI logging megabytes must not be held in full
    // for that. Chunked reads, not lines: a single unterminated line cannot
    // grow the buffer past the bound either.
    let stderr_handle = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stderr = stderr;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > STDERR_TAIL_BYTES * 2 {
                        buf.drain(..buf.len() - STDERR_TAIL_BYTES);
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    // `max_bytes` bounds what a single LINE may retain — not cumulative
    // throughput. Every line is handed to `on_line` and released, so the
    // total streamed volume never lives in memory; a cumulative cap here
    // used to KILL a healthy long run at the 10MB mark and discard the
    // whole turn it was carrying. A line that will not fit is consumed to
    // its newline, dropped, and counted; the run continues.
    let mut reader = BufReader::new(stdout);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut dropped_lines: u64 = 0;

    loop {
        line_buf.clear();
        tokio::select! {
            result = read_line_capped(&mut reader, &mut line_buf, max_bytes) => {
                match result {
                    Ok(CappedLine::Eof) => break,
                    Ok(CappedLine::Line { dropped: true }) => {
                        dropped_lines += 1;
                        warn!(cli = cli_label, max_bytes, "dropped a stdout line larger than the retention cap");
                    }
                    Ok(CappedLine::Line { dropped: false }) => {
                        on_line(String::from_utf8_lossy(&line_buf).trim());
                    }
                    Err(e) => {
                        warn!(cli = cli_label, error = %e, "error reading stdout");
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => {
                kill_process_group(&mut child).await;
                return Ok(SpawnOutcome::Cancelled);
            }
        }
    }

    let status = Box::into_pin(child.wait()).await.map_err(Error::Io)?;
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
        dropped_lines,
    })
}

/// How much stderr is retained (as a tail — see the reader above).
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Outcome of one capped line read.
enum CappedLine {
    /// A complete line is in the buffer — unless `dropped`, in which case the
    /// line exceeded the cap, the buffer is empty, and the line is gone.
    Line { dropped: bool },
    /// End of stream.
    Eof,
}

/// Read one `\n`-terminated line, retaining at most `cap` bytes of it.
///
/// A line that will not fit is not truncated-and-delivered — a cut JSONL
/// event is garbage to every parser downstream — it is consumed to its
/// newline, discarded, and reported as `dropped`. Memory stays bounded by
/// `cap` plus the reader's own buffer, no matter what the child writes.
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<CappedLine> {
    let mut dropped = false;
    loop {
        let (consumed, line_complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                // EOF — a final unterminated line still counts as a line.
                return Ok(if buf.is_empty() && !dropped {
                    CappedLine::Eof
                } else {
                    CappedLine::Line { dropped }
                });
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(newline) => {
                    if !dropped {
                        if buf.len() + newline <= cap {
                            buf.extend_from_slice(&available[..newline]);
                        } else {
                            dropped = true;
                            buf.clear();
                        }
                    }
                    (newline + 1, true)
                }
                None => {
                    let n = available.len();
                    if !dropped {
                        if buf.len() + n <= cap {
                            buf.extend_from_slice(available);
                        } else {
                            dropped = true;
                            buf.clear();
                        }
                    }
                    (n, false)
                }
            }
        };
        reader.consume(consumed);
        if line_complete {
            return Ok(CappedLine::Line { dropped });
        }
    }
}

/// The loss a dropped line represents must reach the consumer, not just the
/// log — every adapter calls this after its spawn completes.
pub(crate) fn warn_dropped_lines(dropped_lines: u64, max_bytes: usize, emit: &dyn Fn(StreamEvent)) {
    if dropped_lines > 0 {
        emit(StreamEvent::Error {
            message: format!(
                "{dropped_lines} output line(s) exceeded the {max_bytes}-byte retention cap and were dropped"
            ),
            severity: Some(crate::events::Severity::Warning),
        });
    }
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

/// Kill the child AND everything it spawned.
///
/// `TokioChildWrapper::kill` dispatches to whichever group mechanism was wrapped
/// on at spawn — the process group on unix, the Job Object on Windows — so the
/// `#[cfg]` that used to live here is gone. It returns a boxed future, hence the
/// pin.
async fn kill_process_group(child: &mut Box<dyn TokioChildWrapper>) {
    let _ = Box::into_pin(child.kill()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_env_exposes_only_explicit_child_variables() {
        let args = vec![
            "-c".to_string(),
            "printf '%s|%s\\n' \"${HOME-unset}\" \"${OWNED-unset}\"".to_string(),
        ];
        let cancel = tokio_util::sync::CancellationToken::new();
        let extra_env = HashMap::from([("OWNED".to_string(), "yes".to_string())]);
        let mut lines = Vec::new();

        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &extra_env,
                clear_env: true,
                strip_env: &[],
                cwd: ".",
                max_bytes: 1024,
                cancel: &cancel,
            },
            |line| lines.push(line.to_string()),
        )
        .await
        .expect("spawn should succeed");

        assert!(matches!(
            outcome,
            SpawnOutcome::Done {
                exit_code: Some(0),
                ..
            }
        ));
        assert_eq!(lines, vec!["unset|yes"]);
    }

    /// CANCELLING TAKES THE WHOLE TREE, not just the process we spawned.
    ///
    /// This is the contract `setpgid`/`killpg` existed to provide, and it had no
    /// test — so the swap to `process-wrap` would have been unverifiable, and so
    /// would any future change to it. It matters because `claude` is a
    /// launcher: the work runs in node processes underneath. Killing only the
    /// parent leaves those alive, holding a model session, writing to a pipe
    /// nobody is reading.
    ///
    /// HOW IT PROVES IT WITHOUT TIMING GAMES: the shell writes a marker file,
    /// spawns a grandchild that would DELETE that file after a delay, then
    /// sleeps. Cancel immediately. If the group died, the grandchild never runs
    /// and the marker survives. If only the parent died, the orphan wakes up and
    /// removes it. The assertion is on a filesystem fact, not on a pid still
    /// being enumerable, which is what makes it honest on both platforms.
    ///
    /// Unix-only for now: it needs a shell that can background a process, and
    /// the Windows equivalent (`cmd /c start`) has different semantics worth
    /// writing deliberately rather than transliterating. The Job Object path is
    /// exercised by CI compiling this file for Windows; that it KILLS the tree
    /// there is not yet proved. Marked plainly rather than assumed.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_kills_the_grandchild_not_just_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survivor");
        std::fs::write(&marker, "alive").unwrap();

        // Grandchild removes the marker after 3s; parent then sleeps 10s.
        let script = format!("(sleep 3; rm -f '{}') & sleep 10", marker.display());
        let args = vec!["-c".to_string(), script];
        let cancel = tokio_util::sync::CancellationToken::new();

        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            token.cancel();
        });

        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                clear_env: false,
                strip_env: &[],
                cwd: dir.path().to_str().unwrap(),
                max_bytes: 1024,
                cancel: &cancel,
            },
            |_: &str| {},
        )
        .await
        .expect("spawn");

        assert!(
            matches!(outcome, SpawnOutcome::Cancelled),
            "run was cancelled"
        );

        // Past when the grandchild would have deleted it, had it survived.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert!(
            marker.exists(),
            "the grandchild outlived cancellation and deleted the marker — the kill did not reach the process group"
        );
    }

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
                clear_env: false,
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
                clear_env: false,
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

    /// A LONG RUN IS NOT AN ERROR. `max_bytes` used to count cumulative
    /// throughput and KILL the process when the total crossed it — but every
    /// line is handed to `on_line` and dropped, so the total was never held in
    /// memory at all. A 30-minute agent run streaming tens of MB of tool
    /// events died at the 10MB mark with its entire turn discarded, which is
    /// how CueFrame's Director lost long turns in production. The cap bounds
    /// what a single line may RETAIN; the run itself must complete.
    #[cfg(unix)]
    #[tokio::test]
    async fn total_output_beyond_max_bytes_streams_through_and_completes() {
        // 200 lines × ~100 bytes ≈ 20 KB through a 1 KB cap.
        let script = "i=0; while [ $i -lt 200 ]; do printf '%0100d\\n' $i; i=$((i+1)); done";
        let args = vec!["-c".to_string(), script.to_string()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut lines = 0u32;
        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                clear_env: false,
                strip_env: &[],
                cwd: ".",
                max_bytes: 1024,
                cancel: &cancel,
            },
            |_| lines += 1,
        )
        .await
        .expect("a large-but-line-bounded run must not be an error");

        match outcome {
            SpawnOutcome::Done { exit_code, .. } => assert_eq!(exit_code, Some(0)),
            SpawnOutcome::Cancelled => panic!("nothing cancelled this run"),
        }
        assert_eq!(lines, 200, "every line was streamed through");
    }

    /// One oversized line loses ITSELF, not the run. The line that cannot be
    /// retained within `max_bytes` is dropped (a truncated JSON event would be
    /// garbage anyway); the lines after it still arrive and the process still
    /// reports its own exit.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_oversized_line_is_dropped_and_the_run_continues() {
        let script = "echo before; printf '%05000d\\n' 7; echo after";
        let args = vec!["-c".to_string(), script.to_string()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut seen: Vec<String> = Vec::new();
        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                clear_env: false,
                strip_env: &[],
                cwd: ".",
                max_bytes: 1024,
                cancel: &cancel,
            },
            |l| seen.push(l.to_string()),
        )
        .await
        .expect("an oversized line must not abort the run");

        assert_eq!(seen, vec!["before".to_string(), "after".to_string()]);
        match outcome {
            SpawnOutcome::Done {
                exit_code,
                dropped_lines,
                ..
            } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(dropped_lines, 1, "the loss is counted, never silent");
            }
            SpawnOutcome::Cancelled => panic!("nothing cancelled this run"),
        }
    }

    /// stderr retention is a TAIL, not the whole stream. It exists so
    /// `extract_error_message` has the CLI's parting words; a chatty process
    /// logging megabytes to stderr must not be held in full for that.
    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_retains_a_bounded_tail() {
        // ~1 MB of filler, then the line that matters, all on stderr.
        let script = "i=0; while [ $i -lt 10000 ]; do printf '%0100d\\n' $i 1>&2; i=$((i+1)); done; \
             echo 'Error: the last words' 1>&2; exit 1";
        let args = vec!["-c".to_string(), script.to_string()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let outcome = spawn_and_stream(
            SpawnParams {
                cli_label: "test",
                binary: "sh",
                args: &args,
                extra_env: &HashMap::new(),
                clear_env: false,
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
            SpawnOutcome::Done { stderr, .. } => {
                let stderr = stderr.expect("stderr was written");
                assert!(
                    stderr.len() <= 256 * 1024,
                    "stderr retention must be bounded, got {} bytes",
                    stderr.len()
                );
                assert!(
                    stderr.contains("the last words"),
                    "the tail is the part that explains the failure"
                );
            }
            SpawnOutcome::Cancelled => panic!("nothing cancelled this run"),
        }
    }
}
