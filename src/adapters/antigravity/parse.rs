use crate::events::{Severity, StreamEvent};
use crate::types::{CliName, RunStats};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct ParseState {
    pub result_text: Option<String>,
    pub session_id: Option<String>,
    pub stats: Option<RunStats>,
    pub success: Option<bool>,
    pub error: Option<String>,
    active_tools: HashSet<String>,
    tool_calls: u32,
}

pub(super) fn parse_line(line: &str, state: &mut ParseState, emit: &dyn Fn(StreamEvent)) {
    if line.is_empty() {
        return;
    }

    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return,
    };
    match parsed.get("event").and_then(|value| value.as_str()) {
        Some("init") => parse_init(&parsed, state, emit),
        Some("step_update") => parse_step_update(&parsed, state, emit),
        Some("result") => parse_result(&parsed, state, emit),
        _ => emit(StreamEvent::Raw {
            provider: CliName::Antigravity,
            event: parsed,
        }),
    }
}

fn parse_init(parsed: &serde_json::Value, state: &mut ParseState, emit: &dyn Fn(StreamEvent)) {
    set_session_id(parsed, state);
    emit(StreamEvent::Raw {
        provider: CliName::Antigravity,
        event: parsed.clone(),
    });
}

fn parse_step_update(
    parsed: &serde_json::Value,
    state: &mut ParseState,
    emit: &dyn Fn(StreamEvent),
) {
    let Some(update) = parsed.get("step_update") else {
        return;
    };
    set_session_id(update, state);

    match update.get("step_type").and_then(|value| value.as_str()) {
        Some("agent_response") => {
            if let Some(delta) = update.get("text_delta").and_then(|value| value.as_str()) {
                if !delta.is_empty() {
                    state
                        .result_text
                        .get_or_insert_with(String::new)
                        .push_str(delta);
                    emit(StreamEvent::TextDelta {
                        text: delta.to_string(),
                    });
                }
            }
        }
        Some("tool") => parse_tool_update(update, state, emit),
        _ => emit(StreamEvent::Raw {
            provider: CliName::Antigravity,
            event: parsed.clone(),
        }),
    }
}

fn parse_tool_update(
    update: &serde_json::Value,
    state: &mut ParseState,
    emit: &dyn Fn(StreamEvent),
) {
    let tool_id = update
        .get("step_index")
        .and_then(|value| value.as_u64())
        .map(|index| format!("step-{index}"))
        .unwrap_or_else(|| "step-unknown".into());
    let tool_info = update.get("tool_info");
    let tool_name = update
        .get("tool_name")
        .or_else(|| tool_info.and_then(|info| info.get("name")))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();

    if state.active_tools.insert(tool_id.clone()) {
        state.tool_calls = state.tool_calls.saturating_add(1);
        let args = tool_info
            .and_then(|info| info.get("parameters"))
            .and_then(|value| value.as_object())
            .map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>()
            });
        emit(StreamEvent::ToolStart {
            tool_name,
            tool_id: tool_id.clone(),
            args,
        });
    }

    if update.get("state").and_then(|value| value.as_str()) == Some("DONE") {
        state.active_tools.remove(&tool_id);
        let error = tool_info
            .and_then(|info| info.get("error"))
            .and_then(error_message);
        let output = tool_info
            .and_then(|info| info.get("output"))
            .map(value_to_string);
        emit(StreamEvent::ToolEnd {
            tool_id,
            success: error.is_none(),
            output,
            error,
        });
    }
}

fn parse_result(parsed: &serde_json::Value, state: &mut ParseState, emit: &dyn Fn(StreamEvent)) {
    let Some(result) = parsed.get("result") else {
        return;
    };
    set_session_id(result, state);

    if let Some(response) = result.get("response").and_then(|value| value.as_str()) {
        state.result_text = Some(response.to_string());
    }
    let status = result
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("INVALID");
    state.success = Some(status == "SUCCESS");

    if let Some(message) = result.get("error").and_then(error_message) {
        state.error = Some(message.clone());
        emit(StreamEvent::Error {
            message,
            severity: Some(Severity::Error),
        });
    }

    if let Some(usage) = result.get("usage").and_then(|value| value.as_object()) {
        state.stats = Some(RunStats {
            input_tokens: usage.get("input_tokens").and_then(|value| value.as_u64()),
            output_tokens: usage.get("output_tokens").and_then(|value| value.as_u64()),
            total_tokens: usage.get("total_tokens").and_then(|value| value.as_u64()),
            cached_tokens: usage
                .get("cache_read_tokens")
                .and_then(|value| value.as_u64()),
            duration_ms: result
                .get("duration_seconds")
                .and_then(|value| value.as_f64())
                .map(|seconds| (seconds * 1000.0).round() as u64),
            tool_calls: Some(state.tool_calls),
        });
    }

    emit(StreamEvent::TurnEnd);
    emit(StreamEvent::Raw {
        provider: CliName::Antigravity,
        event: parsed.clone(),
    });
}

fn set_session_id(value: &serde_json::Value, state: &mut ParseState) {
    if let Some(id) = value
        .get("conversation_id")
        .and_then(|value| value.as_str())
    {
        state.session_id = Some(id.to_string());
    }
}

fn error_message(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(ToOwned::to_owned).or_else(|| {
        value
            .get("message")
            .and_then(|message| message.as_str())
            .map(ToOwned::to_owned)
    })
}

fn value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
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
        let emit = move |event: StreamEvent| events_clone.lock().unwrap().push(event);
        (emit, events)
    }

    #[test]
    fn parses_init() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        parse_line(
            r#"{"event":"init","conversation_id":"agy-123","init":{"cwd":"/tmp"}}"#,
            &mut state,
            &emit,
        );
        assert_eq!(state.session_id.as_deref(), Some("agy-123"));
        assert!(matches!(
            &events.lock().unwrap()[0],
            StreamEvent::Raw {
                provider: CliName::Antigravity,
                ..
            }
        ));
    }

    #[test]
    fn accumulates_agent_response_deltas() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        for line in [
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hello "}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"DONE","step_type":"agent_response","text_delta":"world"}}"#,
        ] {
            parse_line(line, &mut state, &emit);
        }
        assert_eq!(state.result_text.as_deref(), Some("Hello world"));
        assert_eq!(events.lock().unwrap().len(), 2);
    }

    #[test]
    fn done_only_tool_emits_start_and_end() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        parse_line(
            r#"{"event":"step_update","step_update":{"step_index":4,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"},"output":"hi\n"}}}"#,
            &mut state,
            &emit,
        );
        let events = events.lock().unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::ToolStart { tool_name, tool_id, .. }
                if tool_name == "run_command" && tool_id == "step-4"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolEnd { success: true, output, .. }
                if output.as_deref() == Some("hi\n")
        ));
    }

    #[test]
    fn parses_successful_result_and_usage() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        parse_line(
            r#"{"event":"result","result":{"conversation_id":"agy-1","status":"SUCCESS","response":"Done\n","duration_seconds":1.25,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_tokens":3,"total_tokens":15}}}"#,
            &mut state,
            &emit,
        );
        assert_eq!(state.success, Some(true));
        assert_eq!(state.result_text.as_deref(), Some("Done\n"));
        assert_eq!(state.session_id.as_deref(), Some("agy-1"));
        let stats = state.stats.unwrap();
        assert_eq!(stats.input_tokens, Some(10));
        assert_eq!(stats.cached_tokens, Some(3));
        assert_eq!(stats.duration_ms, Some(1250));
        let events = events.lock().unwrap();
        assert!(matches!(events[0], StreamEvent::TurnEnd));
        assert!(matches!(
            &events[1],
            StreamEvent::Raw {
                provider: CliName::Antigravity,
                ..
            }
        ));
    }

    #[test]
    fn parses_failed_result() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        parse_line(
            r#"{"event":"result","result":{"status":"ERROR","response":"","error":"authentication required","usage":{}}}"#,
            &mut state,
            &emit,
        );
        assert_eq!(state.success, Some(false));
        assert_eq!(state.error.as_deref(), Some("authentication required"));
        assert!(matches!(
            &events.lock().unwrap()[0],
            StreamEvent::Error { message, .. } if message == "authentication required"
        ));
    }

    #[test]
    fn ignores_invalid_json() {
        let (emit, events) = collector();
        let mut state = ParseState::default();
        parse_line("not json", &mut state, &emit);
        assert!(events.lock().unwrap().is_empty());
    }
}
