use crate::events::{Severity, StreamEvent};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Internal wrapper that tracks tool failures, idle timeout, and total timeout.
///
/// Uses a monotonic clock ([`Instant`]) for all elapsed-time tracking to avoid
/// issues with system clock adjustments.
pub(super) struct EmitWrapper {
    on_event: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
    max_tool_failures: u32,
    consecutive_failures: Arc<AtomicU32>,
    active_tools: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Elapsed ms since `epoch` of the last activity event.
    last_activity_ms: Arc<AtomicU64>,
    /// Monotonic reference point for all timing.
    epoch: Instant,
    cancel: CancellationToken,
    idle_cancel: CancellationToken,
    total_cancel: CancellationToken,
}

impl EmitWrapper {
    pub(super) fn new(
        on_event: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
        idle_timeout_ms: u64,
        total_timeout_ms: Option<u64>,
        max_tool_failures: u32,
        cancel: CancellationToken,
    ) -> Self {
        let epoch = Instant::now();
        let last_activity_ms = Arc::new(AtomicU64::new(0));
        let idle_cancel = CancellationToken::new();
        let total_cancel = CancellationToken::new();

        // Idle timeout task — resets deadline on every activity event
        if idle_timeout_ms > 0 {
            let last = last_activity_ms.clone();
            let cancel_clone = cancel.clone();
            let on_event_clone = on_event.clone();
            let idle_cancel_clone = idle_cancel.clone();
            let idle_dur = std::time::Duration::from_millis(idle_timeout_ms);

            tokio::spawn(async move {
                loop {
                    // Sleep until the current deadline (last activity + idle duration)
                    let last_ms = last.load(Ordering::Relaxed);
                    let deadline = epoch + std::time::Duration::from_millis(last_ms) + idle_dur;
                    let sleep_until = tokio::time::Instant::from_std(deadline);

                    tokio::select! {
                        _ = tokio::time::sleep_until(sleep_until) => {
                            // Re-check: activity may have occurred while we were sleeping
                            let current_last_ms = last.load(Ordering::Relaxed);
                            let now_ms = epoch.elapsed().as_millis() as u64;
                            if now_ms - current_last_ms >= idle_timeout_ms {
                                if let Some(ref cb) = on_event_clone {
                                    cb(StreamEvent::Error {
                                        message: format!("Idle timeout ({idle_timeout_ms}ms) exceeded"),
                                        severity: Some(Severity::Error),
                                    });
                                }
                                cancel_clone.cancel();
                                return;
                            }
                            // Otherwise, activity happened — loop and recalculate deadline
                        }
                        _ = idle_cancel_clone.cancelled() => return,
                    }
                }
            });
        }

        // Total timeout task
        if let Some(total_ms) = total_timeout_ms {
            let cancel_clone = cancel.clone();
            let on_event_clone = on_event.clone();
            let total_cancel_clone = total_cancel.clone();

            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(total_ms)) => {
                        if let Some(ref cb) = on_event_clone {
                            cb(StreamEvent::Error {
                                message: format!("Total timeout ({total_ms}ms) exceeded"),
                                severity: Some(Severity::Error),
                            });
                        }
                        cancel_clone.cancel();
                    }
                    _ = total_cancel_clone.cancelled() => {}
                }
            });
        }

        Self {
            on_event,
            max_tool_failures,
            consecutive_failures: Arc::new(AtomicU32::new(0)),
            active_tools: Arc::new(std::sync::Mutex::new(HashMap::new())),
            last_activity_ms,
            epoch,
            cancel,
            idle_cancel,
            total_cancel,
        }
    }

    pub(super) fn make_emit_fn(&self) -> impl Fn(StreamEvent) + Send + Sync + '_ {
        move |event: StreamEvent| {
            let elapsed = self.epoch.elapsed().as_millis() as u64;
            self.last_activity_ms.store(elapsed, Ordering::Relaxed);

            match &event {
                StreamEvent::ToolStart {
                    tool_id, tool_name, ..
                } => {
                    let mut tools = self.active_tools.lock().unwrap_or_else(|e| e.into_inner());
                    tools.insert(tool_id.clone(), tool_name.clone());
                }
                StreamEvent::ToolEnd {
                    tool_id, success, ..
                } => {
                    let tool_name = {
                        let mut tools = self.active_tools.lock().unwrap_or_else(|e| e.into_inner());
                        tools.remove(tool_id).unwrap_or_else(|| "unknown".into())
                    };

                    if !success {
                        let count = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                        if count >= self.max_tool_failures {
                            if let Some(ref cb) = self.on_event {
                                cb(StreamEvent::Error {
                                    message: format!(
                                        "Tool \"{tool_name}\" failed {count} consecutive times — aborting"
                                    ),
                                    severity: Some(Severity::Error),
                                });
                            }
                            self.cancel.cancel();
                            return;
                        }
                    } else {
                        self.consecutive_failures.store(0, Ordering::Relaxed);
                    }
                }
                _ => {}
            }

            if let Some(ref cb) = self.on_event {
                cb(event);
            }
        }
    }

    pub(super) fn cleanup(&self) {
        self.idle_cancel.cancel();
        self.total_cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn event_collector() -> (
        Arc<dyn Fn(StreamEvent) + Send + Sync>,
        Arc<Mutex<Vec<StreamEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let cb: Arc<dyn Fn(StreamEvent) + Send + Sync> = Arc::new(move |e: StreamEvent| {
            events_clone.lock().unwrap().push(e);
        });
        (cb, events)
    }

    #[tokio::test]
    async fn emit_wrapper_tracks_tools() {
        let cancel = CancellationToken::new();
        let (cb, events) = event_collector();
        let wrapper = EmitWrapper::new(Some(cb), 0, None, 3, cancel.clone());
        let emit = wrapper.make_emit_fn();

        emit(StreamEvent::ToolStart {
            tool_name: "read".into(),
            tool_id: "t1".into(),
            args: None,
        });
        emit(StreamEvent::ToolEnd {
            tool_id: "t1".into(),
            success: true,
            output: Some("ok".into()),
            error: None,
        });

        wrapper.cleanup();

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 2);
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test]
    async fn emit_wrapper_aborts_on_consecutive_failures() {
        let cancel = CancellationToken::new();
        let (cb, events) = event_collector();
        let wrapper = EmitWrapper::new(Some(cb), 0, None, 2, cancel.clone());
        let emit = wrapper.make_emit_fn();

        // Register tools
        emit(StreamEvent::ToolStart {
            tool_name: "cmd".into(),
            tool_id: "t1".into(),
            args: None,
        });
        emit(StreamEvent::ToolEnd {
            tool_id: "t1".into(),
            success: false,
            output: None,
            error: Some("fail".into()),
        });

        assert!(!cancel.is_cancelled()); // 1 failure, threshold is 2

        emit(StreamEvent::ToolStart {
            tool_name: "cmd".into(),
            tool_id: "t2".into(),
            args: None,
        });
        emit(StreamEvent::ToolEnd {
            tool_id: "t2".into(),
            success: false,
            output: None,
            error: Some("fail again".into()),
        });

        assert!(cancel.is_cancelled()); // 2 consecutive failures → abort

        wrapper.cleanup();

        let evts = events.lock().unwrap();
        // Should have: ToolStart, ToolEnd, ToolStart, Error (abort message — ToolEnd not forwarded after cancel)
        let error_count = evts
            .iter()
            .filter(|e| matches!(e, StreamEvent::Error { .. }))
            .count();
        assert!(error_count >= 1);
    }

    #[tokio::test]
    async fn emit_wrapper_resets_on_success() {
        let cancel = CancellationToken::new();
        let (cb, _events) = event_collector();
        let wrapper = EmitWrapper::new(Some(cb), 0, None, 3, cancel.clone());
        let emit = wrapper.make_emit_fn();

        // Fail once
        emit(StreamEvent::ToolStart {
            tool_name: "a".into(),
            tool_id: "t1".into(),
            args: None,
        });
        emit(StreamEvent::ToolEnd {
            tool_id: "t1".into(),
            success: false,
            output: None,
            error: None,
        });
        assert_eq!(wrapper.consecutive_failures.load(Ordering::Relaxed), 1);

        // Succeed — resets counter
        emit(StreamEvent::ToolStart {
            tool_name: "b".into(),
            tool_id: "t2".into(),
            args: None,
        });
        emit(StreamEvent::ToolEnd {
            tool_id: "t2".into(),
            success: true,
            output: None,
            error: None,
        });
        assert_eq!(wrapper.consecutive_failures.load(Ordering::Relaxed), 0);

        assert!(!cancel.is_cancelled());
        wrapper.cleanup();
    }

    #[tokio::test]
    async fn emit_wrapper_total_timeout() {
        let cancel = CancellationToken::new();
        let (cb, events) = event_collector();
        let wrapper = EmitWrapper::new(Some(cb), 0, Some(50), 3, cancel.clone());

        // Wait for timeout to fire
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(cancel.is_cancelled());
        let evts = events.lock().unwrap();
        assert!(evts.iter().any(
            |e| matches!(e, StreamEvent::Error { message, .. } if message.contains("Total timeout"))
        ));

        wrapper.cleanup();
    }

    #[tokio::test]
    async fn emit_wrapper_idle_timeout() {
        let cancel = CancellationToken::new();
        let (cb, events) = event_collector();
        // 50ms idle timeout, check every 1s — but we need it to fire quickly
        // The idle check runs every 1s, so this test uses a short idle value
        // and waits long enough for one check cycle
        let wrapper = EmitWrapper::new(Some(cb), 50, None, 3, cancel.clone());

        // Don't send any events — idle should fire after ~1s check
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        assert!(cancel.is_cancelled());
        let evts = events.lock().unwrap();
        assert!(evts.iter().any(
            |e| matches!(e, StreamEvent::Error { message, .. } if message.contains("Idle timeout"))
        ));

        wrapper.cleanup();
    }
}
