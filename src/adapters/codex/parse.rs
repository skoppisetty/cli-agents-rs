use crate::events::{Severity, StreamEvent};
use crate::types::{CliName, RunStats};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ParseState {
    pub result_text: Option<String>,
    pub session_id: Option<String>,
    pub stats: Option<RunStats>,
    pub failed: bool,
}

pub(super) fn parse_line(
    line: &str,
    state: &mut ParseState,
    text_tracker: &mut HashMap<String, String>,
    emit: &dyn Fn(StreamEvent),
) {
    if line.is_empty() {
        return;
    }

    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    let msg_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "thread.started" => {
            if let Some(tid) = parsed.get("thread_id").and_then(|v| v.as_str()) {
                state.session_id = Some(tid.to_string());
            }
            emit(StreamEvent::Raw {
                provider: CliName::Codex,
                event: parsed,
            });
        }

        "turn.started" => {
            emit(StreamEvent::Raw {
                provider: CliName::Codex,
                event: parsed,
            });
        }

        "turn.completed" => {
            if let Some(usage) = parsed.get("usage").and_then(|v| v.as_object()) {
                state.stats = Some(RunStats {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    total_tokens: None,
                    cached_tokens: usage.get("cached_input_tokens").and_then(|v| v.as_u64()),
                    duration_ms: None,
                    tool_calls: None,
                });
            }
            emit(StreamEvent::TurnEnd);
        }

        "turn.failed" => {
            state.failed = true;
            let message = parsed
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("turn failed")
                .to_string();
            emit(StreamEvent::Error {
                message,
                severity: Some(Severity::Error),
            });
        }

        "item.started" | "item.updated" | "item.completed" => {
            parse_item_event(&parsed, msg_type, state, text_tracker, emit);
        }

        "error" => {
            let message = parsed
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            emit(StreamEvent::Error {
                message,
                severity: Some(Severity::Error),
            });
            state.failed = true;
        }

        _ => {
            emit(StreamEvent::Raw {
                provider: CliName::Codex,
                event: parsed,
            });
        }
    }
}

fn parse_item_event(
    parsed: &serde_json::Value,
    event_type: &str,
    state: &mut ParseState,
    text_tracker: &mut HashMap<String, String>,
    emit: &dyn Fn(StreamEvent),
) {
    let item = match parsed.get("item") {
        Some(i) => i,
        None => return,
    };

    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let item_id = item
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match item_type {
        "agent_message" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "item.updated" || event_type == "item.completed" {
                let prev = text_tracker.get(&item_id).cloned().unwrap_or_default();
                if text.len() > prev.len() {
                    if let Some(delta) = text.get(prev.len()..) {
                        emit(StreamEvent::TextDelta {
                            text: delta.to_string(),
                        });
                    }
                }
                text_tracker.insert(item_id.clone(), text.to_string());
            }

            if event_type == "item.completed" {
                state.result_text = Some(text.to_string());
            }
        }

        "reasoning" => {
            if event_type == "item.completed" {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    emit(StreamEvent::ThinkingDelta {
                        text: text.to_string(),
                    });
                }
            }
        }

        "command_execution" => {
            if event_type == "item.started" {
                let command = item
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut args = HashMap::new();
                args.insert("command".to_string(), serde_json::Value::String(command));
                emit(StreamEvent::ToolStart {
                    tool_name: "command_execution".into(),
                    tool_id: item_id,
                    args: Some(args),
                });
            } else if event_type == "item.completed" {
                let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
                let output = item
                    .get("aggregated_output")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                emit(StreamEvent::ToolEnd {
                    tool_id: item_id,
                    success: exit_code == Some(0),
                    output,
                    error: None,
                });
            }
        }

        "mcp_tool_call" => {
            if event_type == "item.started" {
                let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("mcp");
                let tool = item
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let tool_name = format!("{server}:{tool}");
                let args = item.get("arguments").and_then(|v| {
                    v.as_object().map(|m| {
                        m.iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect::<HashMap<String, serde_json::Value>>()
                    })
                });
                emit(StreamEvent::ToolStart {
                    tool_name,
                    tool_id: item_id,
                    args,
                });
            } else if event_type == "item.completed" {
                let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let error = item
                    .get("error")
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                emit(StreamEvent::ToolEnd {
                    tool_id: item_id,
                    success: status == "completed",
                    output: None,
                    error,
                });
            }
        }

        _ => {
            emit(StreamEvent::Raw {
                provider: CliName::Codex,
                event: parsed.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn collector() -> (
        impl Fn(StreamEvent) + Send + Sync,
        Arc<Mutex<Vec<StreamEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let emit = move |e: StreamEvent| {
            events_clone.lock().unwrap().push(e);
        };
        (emit, events)
    }

    #[test]
    fn parse_thread_started() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"thread.started","thread_id":"tid-abc"}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        assert_eq!(state.session_id.as_deref(), Some("tid-abc"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Codex,
                ..
            }
        ));
    }

    #[test]
    fn parse_turn_completed_with_usage() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50,"cached_input_tokens":10}}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        let stats = state.stats.as_ref().unwrap();
        assert_eq!(stats.input_tokens, Some(100));
        assert_eq!(stats.output_tokens, Some(50));
        assert_eq!(stats.cached_tokens, Some(10));

        let evts = events.lock().unwrap();
        assert!(matches!(&evts[0], StreamEvent::TurnEnd));
    }

    #[test]
    fn parse_turn_failed() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        assert!(state.failed);
        let evts = events.lock().unwrap();
        assert!(
            matches!(&evts[0], StreamEvent::Error { message, .. } if message == "401 Unauthorized")
        );
    }

    #[test]
    fn parse_agent_message_deltas() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        // item.started — no text yet
        let line1 =
            r#"{"type":"item.started","item":{"type":"agent_message","id":"m1","text":""}}"#;
        parse_line(line1, &mut state, &mut tracker, &emit);

        // item.updated — partial text
        let line2 =
            r#"{"type":"item.updated","item":{"type":"agent_message","id":"m1","text":"Hello"}}"#;
        parse_line(line2, &mut state, &mut tracker, &emit);

        // item.updated — more text
        let line3 = r#"{"type":"item.updated","item":{"type":"agent_message","id":"m1","text":"Hello world"}}"#;
        parse_line(line3, &mut state, &mut tracker, &emit);

        // item.completed
        let line4 = r#"{"type":"item.completed","item":{"type":"agent_message","id":"m1","text":"Hello world!"}}"#;
        parse_line(line4, &mut state, &mut tracker, &emit);

        assert_eq!(state.result_text.as_deref(), Some("Hello world!"));

        let evts = events.lock().unwrap();
        // updated: "Hello", updated delta: " world", completed delta: "!"
        let text_deltas: Vec<_> = evts
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Hello", " world", "!"]);
    }

    #[test]
    fn parse_command_execution() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line1 = r#"{"type":"item.started","item":{"type":"command_execution","id":"c1","command":"ls -la"}}"#;
        parse_line(line1, &mut state, &mut tracker, &emit);

        let line2 = r#"{"type":"item.completed","item":{"type":"command_execution","id":"c1","command":"ls -la","exit_code":0,"aggregated_output":"file1\nfile2"}}"#;
        parse_line(line2, &mut state, &mut tracker, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 2);
        assert!(
            matches!(&evts[0], StreamEvent::ToolStart { tool_name, tool_id, args }
            if tool_name == "command_execution" && tool_id == "c1"
            && args.as_ref().unwrap().get("command").and_then(|v| v.as_str()) == Some("ls -la"))
        );
        assert!(
            matches!(&evts[1], StreamEvent::ToolEnd { tool_id, success, output, .. }
            if tool_id == "c1" && *success && output.as_deref() == Some("file1\nfile2"))
        );
    }

    #[test]
    fn parse_command_execution_failure() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"item.completed","item":{"type":"command_execution","id":"c2","command":"bad","exit_code":1,"aggregated_output":"error msg"}}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        let evts = events.lock().unwrap();
        assert!(matches!(&evts[0], StreamEvent::ToolEnd { success, .. } if !success));
    }

    #[test]
    fn parse_mcp_tool_call() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line1 = r#"{"type":"item.started","item":{"type":"mcp_tool_call","id":"mcp1","server":"myserver","tool":"search","arguments":{"query":"test"}}}"#;
        parse_line(line1, &mut state, &mut tracker, &emit);

        let line2 = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","id":"mcp1","server":"myserver","tool":"search","status":"completed"}}"#;
        parse_line(line2, &mut state, &mut tracker, &emit);

        let evts = events.lock().unwrap();
        assert!(matches!(&evts[0], StreamEvent::ToolStart { tool_name, .. }
            if tool_name == "myserver:search"));
        assert!(matches!(&evts[1], StreamEvent::ToolEnd { success, .. } if *success));
    }

    #[test]
    fn parse_reasoning() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"item.completed","item":{"type":"reasoning","id":"r1","text":"Thinking about the problem..."}}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ThinkingDelta { text } if text == "Thinking about the problem...")
        );
    }

    #[test]
    fn parse_error_event() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let line = r#"{"type":"error","message":"Reconnecting... 1/5 (401)"}"#;
        parse_line(line, &mut state, &mut tracker, &emit);

        assert!(state.failed);
        let evts = events.lock().unwrap();
        assert!(
            matches!(&evts[0], StreamEvent::Error { message, .. } if message.contains("Reconnecting"))
        );
    }

    #[test]
    fn parse_invalid_json() {
        let (emit, collected) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        parse_line("not json", &mut state, &mut tracker, &emit);
        parse_line("", &mut state, &mut tracker, &emit);

        let evts = collected.lock().unwrap();
        assert!(evts.is_empty());
    }

    #[test]
    fn full_codex_conversation() {
        let (emit, _events) = collector();
        let mut state = ParseState::default();
        let mut tracker = HashMap::new();

        let lines = [
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"type":"agent_message","id":"m1","text":""}}"#,
            r#"{"type":"item.updated","item":{"type":"agent_message","id":"m1","text":"Let me"}}"#,
            r#"{"type":"item.updated","item":{"type":"agent_message","id":"m1","text":"Let me check."}}"#,
            r#"{"type":"item.started","item":{"type":"command_execution","id":"c1","command":"echo hi"}}"#,
            r#"{"type":"item.completed","item":{"type":"command_execution","id":"c1","command":"echo hi","exit_code":0,"aggregated_output":"hi"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","id":"m1","text":"Let me check. Done!"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":80,"output_tokens":40}}"#,
        ];

        for line in &lines {
            parse_line(line, &mut state, &mut tracker, &emit);
        }

        assert_eq!(state.session_id.as_deref(), Some("t-1"));
        assert_eq!(state.result_text.as_deref(), Some("Let me check. Done!"));
        assert!(!state.failed);

        let stats = state.stats.as_ref().unwrap();
        assert_eq!(stats.input_tokens, Some(80));
    }
}
