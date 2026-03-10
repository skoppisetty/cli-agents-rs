use crate::events::{Severity, StreamEvent};
use crate::types::{CliName, RunStats};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ParseState {
    pub result_text: Option<String>,
    pub session_id: Option<String>,
    pub stats: Option<RunStats>,
    pub cost_usd: Option<f64>,
    pub success: Option<bool>,
}

pub(super) fn parse_line(
    line: &str,
    state: &mut ParseState,
    active_tools: &mut HashMap<String, String>,
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
        "system" => {
            if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                state.session_id = Some(sid.to_string());
            }
            emit(StreamEvent::Raw {
                provider: CliName::Claude,
                event: parsed,
            });
        }

        "stream_event" => {
            let event = match parsed.get("event") {
                Some(e) => e,
                None => return,
            };
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match event_type {
                "content_block_delta" => {
                    if let Some(delta) = event.get("delta") {
                        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if delta_type == "text_delta" {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                emit(StreamEvent::TextDelta {
                                    text: text.to_string(),
                                });
                                let rt = state.result_text.get_or_insert_with(String::new);
                                rt.push_str(text);
                            }
                        } else if delta_type == "thinking_delta" {
                            if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                                emit(StreamEvent::ThinkingDelta {
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                }

                "content_block_start" => {
                    if let Some(block) = event.get("content_block") {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                            let tool_id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let tool_name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            active_tools.insert(tool_id.clone(), tool_name.clone());
                            emit(StreamEvent::ToolStart {
                                tool_name,
                                tool_id,
                                args: None,
                            });
                        }
                    }
                }

                "message_stop" => {
                    emit(StreamEvent::TurnEnd);
                }

                _ => {}
            }
        }

        "assistant" => {
            // Complete assistant message — extract tool_start for tools not yet seen
            if let Some(message) = parsed.get("message") {
                if let Some(content) = message.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                            let tool_id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let tool_name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !active_tools.contains_key(&tool_id) {
                                active_tools.insert(tool_id.clone(), tool_name.clone());
                                let args = block.get("input").and_then(|v| {
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
                        }
                    }
                }
            }
        }

        "user" => {
            // Tool results
            if let Some(message) = parsed.get("message") {
                if let Some(content) = message.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                            let tool_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            let output = extract_tool_output(block.get("content"));

                            active_tools.remove(&tool_id);
                            let (output, error) = if is_error {
                                (None, output)
                            } else {
                                (output, None)
                            };
                            emit(StreamEvent::ToolEnd {
                                tool_id,
                                success: !is_error,
                                output,
                                error,
                            });
                        }
                    }
                }
            }
        }

        "result" => {
            let subtype = parsed.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            let is_success = subtype == "success";
            state.success = Some(is_success);

            if let Some(result_text) = parsed.get("result").and_then(|v| v.as_str()) {
                state.result_text = Some(result_text.to_string());
            }

            if let Some(usage) = parsed.get("usage").and_then(|v| v.as_object()) {
                state.stats = Some(RunStats {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    total_tokens: None,
                    cached_tokens: usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64()),
                    duration_ms: parsed.get("duration_ms").and_then(|v| v.as_u64()),
                    tool_calls: parsed
                        .get("num_turns")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32),
                });
            }

            state.cost_usd = parsed.get("total_cost_usd").and_then(|v| v.as_f64());

            if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                state.session_id = Some(sid.to_string());
            }

            if !is_success {
                let error_msg = parsed
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_else(|| format!("Claude exited with: {subtype}"));
                emit(StreamEvent::Error {
                    message: error_msg,
                    severity: Some(Severity::Error),
                });
            }
        }

        _ => {
            emit(StreamEvent::Raw {
                provider: CliName::Claude,
                event: parsed,
            });
        }
    }
}

/// Extract text output from a tool_result content block.
fn extract_tool_output(content: Option<&serde_json::Value>) -> Option<String> {
    match content {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(arr)) => {
            let texts: Vec<&str> = arr
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Collects emitted events for assertions.
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
    fn parse_system_message() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"system","session_id":"abc-123","tools":[]}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        assert_eq!(state.session_id.as_deref(), Some("abc-123"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Claude,
                ..
            }
        ));
    }

    #[test]
    fn parse_text_delta() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello "}}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let line2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}}"#;
        parse_line(line2, &mut state, &mut tools, &emit);

        assert_eq!(state.result_text.as_deref(), Some("Hello world"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 2);
        assert!(matches!(&evts[0], StreamEvent::TextDelta { text } if text == "Hello "));
        assert!(matches!(&evts[1], StreamEvent::TextDelta { text } if text == "world"));
    }

    #[test]
    fn parse_thinking_delta() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"Let me think..."}}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ThinkingDelta { text } if text == "Let me think...")
        );
    }

    #[test]
    fn parse_tool_start_from_content_block_start() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"tool_1","name":"read_file"}}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        assert!(tools.contains_key("tool_1"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ToolStart { tool_name, tool_id, .. }
            if tool_name == "read_file" && tool_id == "tool_1")
        );
    }

    #[test]
    fn parse_tool_start_dedup_from_assistant() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        // First: content_block_start emits tool_start
        let line1 = r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"tool_1","name":"read_file"}}}"#;
        parse_line(line1, &mut state, &mut tools, &emit);

        // Then: assistant message with same tool — should NOT emit duplicate
        let line2 = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool_1","name":"read_file","input":{"path":"/tmp"}}]}}"#;
        parse_line(line2, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1); // Only one tool_start
    }

    #[test]
    fn parse_tool_start_from_assistant_only() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        // No content_block_start — tool_start should come from assistant message
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool_2","name":"write_file","input":{"path":"/tmp/out","content":"hi"}}]}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ToolStart { tool_name, tool_id, args }
            if tool_name == "write_file" && tool_id == "tool_2" && args.is_some())
        );
    }

    #[test]
    fn parse_tool_result() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();
        tools.insert("tool_1".to_string(), "read_file".to_string());

        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_1","content":"file contents here","is_error":false}]}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        assert!(!tools.contains_key("tool_1"));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], StreamEvent::ToolEnd { tool_id, success, output, error }
            if tool_id == "tool_1" && *success && output.as_deref() == Some("file contents here") && error.is_none())
        );
    }

    #[test]
    fn parse_tool_result_error() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();
        tools.insert("tool_1".to_string(), "read_file".to_string());

        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_1","content":"Permission denied","is_error":true}]}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert!(
            matches!(&evts[0], StreamEvent::ToolEnd { success, output, error, .. }
            if !success && output.is_none() && error.as_deref() == Some("Permission denied"))
        );
    }

    #[test]
    fn parse_tool_result_array_content() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();
        tools.insert("tool_1".to_string(), "read_file".to_string());

        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_1","content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],"is_error":false}]}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert!(matches!(&evts[0], StreamEvent::ToolEnd { output, .. }
            if output.as_deref() == Some("line1\nline2")));
    }

    #[test]
    fn parse_turn_end() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(&evts[0], StreamEvent::TurnEnd));
    }

    #[test]
    fn parse_result_success() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"result","subtype":"success","result":"The answer is 4","session_id":"sess-1","total_cost_usd":0.005,"duration_ms":1234,"num_turns":3,"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20}}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        assert_eq!(state.success, Some(true));
        assert_eq!(state.result_text.as_deref(), Some("The answer is 4"));
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
        assert!((state.cost_usd.unwrap() - 0.005).abs() < f64::EPSILON);

        let stats = state.stats.as_ref().unwrap();
        assert_eq!(stats.input_tokens, Some(100));
        assert_eq!(stats.output_tokens, Some(50));
        assert_eq!(stats.cached_tokens, Some(20));
        assert_eq!(stats.duration_ms, Some(1234));
        assert_eq!(stats.tool_calls, Some(3));

        let evts = events.lock().unwrap();
        assert!(evts.is_empty()); // Success doesn't emit error
    }

    #[test]
    fn parse_result_error() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"result","subtype":"error","errors":["rate limited","timeout"]}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        assert_eq!(state.success, Some(false));
        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(&evts[0], StreamEvent::Error { message, severity }
            if message == "rate limited; timeout" && *severity == Some(Severity::Error)));
    }

    #[test]
    fn parse_unknown_type_emits_raw() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let line = r#"{"type":"custom_event","data":"hello"}"#;
        parse_line(line, &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        assert!(matches!(
            &evts[0],
            StreamEvent::Raw {
                provider: CliName::Claude,
                ..
            }
        ));
    }

    #[test]
    fn parse_invalid_json_is_ignored() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        parse_line("not json", &mut state, &mut tools, &emit);
        parse_line("", &mut state, &mut tools, &emit);

        let evts = events.lock().unwrap();
        assert!(evts.is_empty());
    }

    #[test]
    fn full_conversation_flow() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        let mut tools = HashMap::new();

        let lines = [
            r#"{"type":"system","session_id":"sess-42","tools":["read_file"]}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"I'll read "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"the file."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"t1","name":"read_file"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"hello world","is_error":false}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"The file says hello."}}}"#,
            r#"{"type":"result","subtype":"success","result":"Done.","session_id":"sess-42","usage":{"input_tokens":50,"output_tokens":25}}"#,
        ];

        for line in &lines {
            parse_line(line, &mut state, &mut tools, &emit);
        }

        assert_eq!(state.session_id.as_deref(), Some("sess-42"));
        assert_eq!(state.success, Some(true));
        // result_text from the result message overrides accumulated text
        assert_eq!(state.result_text.as_deref(), Some("Done."));

        let evts = events.lock().unwrap();
        // Raw(system) + TextDelta + TextDelta + ToolStart + TurnEnd + ToolEnd + TextDelta + (no event for success result)
        assert_eq!(evts.len(), 7);
    }

    #[test]
    fn extract_tool_output_string() {
        let val = serde_json::json!("hello");
        assert_eq!(extract_tool_output(Some(&val)), Some("hello".into()));
    }

    #[test]
    fn extract_tool_output_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "image", "data": "..."},
            {"type": "text", "text": "b"}
        ]);
        assert_eq!(extract_tool_output(Some(&val)), Some("a\nb".into()));
    }

    #[test]
    fn extract_tool_output_none() {
        assert_eq!(extract_tool_output(None), None);
        assert_eq!(extract_tool_output(Some(&serde_json::json!(42))), None);
    }
}
