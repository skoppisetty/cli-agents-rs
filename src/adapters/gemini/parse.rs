use crate::events::{Severity, StreamEvent};
use crate::types::{CliName, RunStats};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ParseState {
    pub result_text: Option<String>,
    pub session_id: Option<String>,
    pub stats: Option<RunStats>,
}

pub(super) fn parse_line(line: &str, state: &mut ParseState, emit: &dyn Fn(StreamEvent)) {
    if line.is_empty() {
        return;
    }

    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    let msg_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "init" => {
            if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                state.session_id = Some(sid.to_string());
            }
            emit(StreamEvent::Raw {
                provider: CliName::Gemini,
                event: parsed,
            });
        }

        "message" => {
            let role = parsed.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = parsed.get("content").and_then(|v| v.as_str());

            if role == "assistant" {
                if let Some(text) = content {
                    emit(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                    if parsed.get("delta").is_some() {
                        let rt = state.result_text.get_or_insert_with(String::new);
                        rt.push_str(text);
                    } else {
                        state.result_text = Some(text.to_string());
                    }
                }
            }
        }

        "tool_use" => {
            let tool_name = parsed
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool_id = parsed
                .get("tool_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = parsed.get("parameters").and_then(|v| {
                v.as_object().map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<HashMap<String, serde_json::Value>>()
                })
            });
            emit(StreamEvent::ToolStart {
                tool_name,
                tool_id,
                args,
            });
        }

        "tool_result" => {
            let tool_id = parsed
                .get("tool_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = parsed.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let success = status == "success";
            let output = parsed
                .get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let error = parsed
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            emit(StreamEvent::ToolEnd {
                tool_id,
                success,
                output,
                error,
            });
        }

        "error" => {
            let message = parsed
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            let severity = parsed
                .get("severity")
                .and_then(|v| v.as_str())
                .and_then(|s| match s {
                    "warning" => Some(Severity::Warning),
                    "error" => Some(Severity::Error),
                    _ => None,
                });

            emit(StreamEvent::Error { message, severity });
        }

        "result" => {
            if let Some(stats_obj) = parsed.get("stats").and_then(|v| v.as_object()) {
                state.stats = Some(RunStats {
                    input_tokens: stats_obj.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: stats_obj.get("output_tokens").and_then(|v| v.as_u64()),
                    total_tokens: stats_obj.get("total_tokens").and_then(|v| v.as_u64()),
                    cached_tokens: stats_obj.get("cached").and_then(|v| v.as_u64()),
                    duration_ms: stats_obj.get("duration_ms").and_then(|v| v.as_u64()),
                    tool_calls: stats_obj
                        .get("tool_calls")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32),
                });
            }
            emit(StreamEvent::Raw {
                provider: CliName::Gemini,
                event: parsed,
            });
        }

        _ => {
            emit(StreamEvent::Raw {
                provider: CliName::Gemini,
                event: parsed,
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
    fn parse_init() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"init","session_id":"gem-123"}"#;
        parse_line(line, &mut state, &emit);

        assert_eq!(state.session_id.as_deref(), Some("gem-123"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Gemini,
                ..
            }
        ));
    }

    #[test]
    fn parse_assistant_message_delta() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        // Delta message — accumulates text
        let line = r#"{"type":"message","role":"assistant","content":"Hello ","delta":true}"#;
        parse_line(line, &mut state, &emit);

        let line2 = r#"{"type":"message","role":"assistant","content":"world","delta":true}"#;
        parse_line(line2, &mut state, &emit);

        assert_eq!(state.result_text.as_deref(), Some("Hello world"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 2);
        assert!(matches!(&evts[0], StreamEvent::TextDelta { text } if text == "Hello "));
        assert!(matches!(&evts[1], StreamEvent::TextDelta { text } if text == "world"));
    }

    #[test]
    fn parse_assistant_message_full() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        // Non-delta message — replaces text
        let line = r#"{"type":"message","role":"assistant","content":"Complete answer."}"#;
        parse_line(line, &mut state, &emit);

        assert_eq!(state.result_text.as_deref(), Some("Complete answer."));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
    }

    #[test]
    fn parse_user_message_ignored() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"message","role":"user","content":"My question"}"#;
        parse_line(line, &mut state, &emit);

        assert!(state.result_text.is_none());
        let evts = events.lock().unwrap();
        assert!(evts.is_empty());
    }

    #[test]
    fn parse_tool_use() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"tool_use","tool_name":"shell","tool_id":"gt1","parameters":{"command":"ls"}}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        match &evts[0] {
            StreamEvent::ToolStart {
                tool_name,
                tool_id,
                args,
            } => {
                assert_eq!(tool_name, "shell");
                assert_eq!(tool_id, "gt1");
                assert!(args.is_some());
                assert_eq!(
                    args.as_ref()
                        .unwrap()
                        .get("command")
                        .and_then(|v| v.as_str()),
                    Some("ls")
                );
            }
            _ => panic!("expected ToolStart"),
        }
    }

    #[test]
    fn parse_tool_result_success() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"tool_result","tool_id":"gt1","status":"success","output":"file1.txt\nfile2.txt"}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ToolEnd { tool_id, success, output, error }
            if tool_id == "gt1" && *success && output.as_deref() == Some("file1.txt\nfile2.txt") && error.is_none())
        );
    }

    #[test]
    fn parse_tool_result_failure() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"tool_result","tool_id":"gt1","status":"error","error":{"message":"Permission denied"}}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert!(
            matches!(&evts[0], StreamEvent::ToolEnd { success, error, .. }
            if !success && error.as_deref() == Some("Permission denied"))
        );
    }

    #[test]
    fn parse_error_event() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"error","message":"Rate limited","severity":"warning"}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(&evts[0], StreamEvent::Error { message, severity }
            if message == "Rate limited" && *severity == Some(Severity::Warning)));
    }

    #[test]
    fn parse_error_default_severity() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"error","message":"Something broke"}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert!(matches!(&evts[0], StreamEvent::Error { severity, .. }
            if severity.is_none()));
    }

    #[test]
    fn parse_result_with_stats() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"result","stats":{"input_tokens":200,"output_tokens":100,"total_tokens":300,"cached":50,"duration_ms":5000,"tool_calls":4}}"#;
        parse_line(line, &mut state, &emit);

        let stats = state.stats.as_ref().unwrap();
        assert_eq!(stats.input_tokens, Some(200));
        assert_eq!(stats.output_tokens, Some(100));
        assert_eq!(stats.total_tokens, Some(300));
        assert_eq!(stats.cached_tokens, Some(50));
        assert_eq!(stats.duration_ms, Some(5000));
        assert_eq!(stats.tool_calls, Some(4));

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Gemini,
                ..
            }
        ));
    }

    #[test]
    fn parse_unknown_type() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let line = r#"{"type":"debug","data":"trace info"}"#;
        parse_line(line, &mut state, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Gemini,
                ..
            }
        ));
    }

    #[test]
    fn parse_invalid_json() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        parse_line("not json at all", &mut state, &emit);
        parse_line("", &mut state, &emit);
        parse_line("{incomplete", &mut state, &emit);

        let evts = events.lock().unwrap();
        assert!(evts.is_empty());
    }

    #[test]
    fn full_gemini_conversation() {
        let (emit, events) = collector();
        let mut state = ParseState::default();

        let lines = [
            r#"{"type":"init","session_id":"g-1"}"#,
            r#"{"type":"message","role":"assistant","content":"Let me check. ","delta":true}"#,
            r#"{"type":"tool_use","tool_name":"shell","tool_id":"t1","parameters":{"command":"echo hi"}}"#,
            r#"{"type":"tool_result","tool_id":"t1","status":"success","output":"hi"}"#,
            r#"{"type":"message","role":"assistant","content":"The result is: hi","delta":true}"#,
            r#"{"type":"result","stats":{"input_tokens":100,"output_tokens":50}}"#,
        ];

        for line in &lines {
            parse_line(line, &mut state, &emit);
        }

        assert_eq!(state.session_id.as_deref(), Some("g-1"));
        assert_eq!(
            state.result_text.as_deref(),
            Some("Let me check. The result is: hi")
        );

        let stats = state.stats.as_ref().unwrap();
        assert_eq!(stats.input_tokens, Some(100));

        let evts = events.lock().unwrap();
        // Raw(init) + TextDelta + ToolStart + ToolEnd + TextDelta + Raw(result) = 6
        assert_eq!(evts.len(), 6);
    }
}
