//! Opt-in official Codex app-server transport. No product journal or automatic resend.
//! Callers persist a prepared thread/client-message binding BEFORE invoking Run.
use crate::{Error, Result, RunOptions, StreamEvent};
use process_wrap::tokio::{TokioChildWrapper, TokioCommandWrap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::{collections::VecDeque, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexTurnBinding {
    pub thread_id: String,
    pub client_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase", deny_unknown_fields)]
pub enum CodexAppServerRequest {
    Prepare {
        options: RunOptions,
    },
    Run {
        options: RunOptions,
        binding: CodexTurnBinding,
    },
    Inspect {
        options: RunOptions,
        binding: CodexTurnBinding,
    },
}
/// Unsupported, missing, malformed, incomplete and transport failures remain uncertain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CodexTurnObservation {
    Prepared {
        thread_id: String,
    },
    Observed {
        binding: CodexTurnBinding,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Uncertain {
        reason: String,
    },
}
impl PartialEq for CodexTurnBinding {
    fn eq(&self, rhs: &Self) -> bool {
        self.thread_id == rhs.thread_id
            && self.client_message_id == rhs.client_message_id
            && self.turn_id == rhs.turn_id
    }
}
fn uncertain(reason: &str) -> CodexTurnObservation {
    CodexTurnObservation::Uncertain {
        reason: reason.into(),
    }
}
fn valid_id(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}

struct Connection {
    child: Box<dyn TokioChildWrapper>,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    notifications: VecDeque<Value>,
    notification_bytes: usize,
    initialized: bool,
    prepared_thread: Option<String>,
    thread_config: Option<Value>,
    next_id: u64,
    cap: usize,
    closed: bool,
    progress: Option<CodexProgress>,
    parse_state: super::parse::ParseState,
    text_tracker: HashMap<String, String>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        // An embedding task can be dropped without polling its cancellation token.
        if !self.closed {
            let _ = self.child.start_kill();
        }
    }
}
impl Connection {
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn read(&mut self) -> Result<Value> {
        let mut line = Vec::new();
        (&mut self.output)
            .take(self.cap as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if line.len() > self.cap || !line.ends_with(b"\n") {
            return Err(Error::Other(
                "Codex app-server closed or exceeded message bound".into(),
            ));
        }
        serde_json::from_slice(&line)
            .map_err(|_| Error::Other("Malformed Codex app-server message".into()))
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.read().await?;
            if value.get("id") == Some(&json!(id)) && value.get("method").is_none() {
                if value.get("error").is_some() {
                    return Err(Error::Other(
                        "Codex app-server request unsupported or refused".into(),
                    ));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| Error::Other("Missing app-server result".into()));
            }
            if let Some(server_id) = value.get("id") {
                // This adapter never silently approves server tool/permission requests.
                self.send(json!({"id":server_id,"error":{"code":-32601,"message":"Unsupported server request"}})).await?;
            } else {
                if self.notifications.len() >= 1024 {
                    return Err(Error::Other("Codex notification bound exceeded".into()));
                }
                let bytes = serde_json::to_vec(&value)?.len();
                if self.notification_bytes.saturating_add(bytes) > self.cap {
                    return Err(Error::Other(
                        "Codex notification byte bound exceeded".into(),
                    ));
                }
                self.notification_bytes += bytes;
                self.notifications.push_back(value);
            }
        }
    }
    async fn inspect(&mut self, binding: &CodexTurnBinding) -> Result<CodexTurnObservation> {
        let read = self
            .request(
                "thread/read",
                json!({"threadId":binding.thread_id,"includeTurns":true}),
            )
            .await?;
        if read.pointer("/thread/id").and_then(Value::as_str) != Some(binding.thread_id.as_str()) {
            return Ok(uncertain("thread_identity_mismatch"));
        }
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut matches = Vec::new();
        for _ in 0..20 {
            let page = self
                .request(
                    "thread/turns/list",
                    json!({"threadId":binding.thread_id,
                "itemsView":"full","limit":50,"sortDirection":"asc","cursor":cursor}),
                )
                .await?;
            let Some(turns) = page.get("data").and_then(Value::as_array) else {
                return Ok(uncertain("malformed_history"));
            };
            if turns.len() > 50 {
                return Ok(uncertain("history_bound_exceeded"));
            }
            for turn in turns {
                // Protocol default for omitted itemsView is full; explicit summaries aren't evidence.
                if turn
                    .get("itemsView")
                    .and_then(Value::as_str)
                    .unwrap_or("full")
                    != "full"
                {
                    return Ok(uncertain("partial_history"));
                }
                let Some(items) = turn.get("items").and_then(Value::as_array) else {
                    return Ok(uncertain("malformed_history"));
                };
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("contextCompaction") {
                        return Ok(uncertain("compacted_history"));
                    }
                    if item.get("type").and_then(Value::as_str) == Some("userMessage")
                        && item.get("clientId").and_then(Value::as_str)
                            == Some(binding.client_message_id.as_str())
                    {
                        matches.push(turn.clone());
                    }
                }
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok(classify(binding, &matches)),
                Some(Value::String(next)) if !next.is_empty() && seen.insert(next.clone()) => {
                    cursor = Some(next.clone())
                }
                _ => return Ok(uncertain("invalid_history_cursor")),
            }
        }
        Ok(uncertain("history_bound_exceeded"))
    }
    async fn operation(&mut self, request: &CodexAppServerRequest) -> Result<CodexTurnObservation> {
        if !self.initialized {
            self.request(
            "initialize",
            json!({"clientInfo":{"name":"cli_agents_provider","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}}),
        )
        .await?;
            self.send(json!({"method":"initialized"})).await?;
            self.initialized = true;
        }
        match request {
            CodexAppServerRequest::Prepare { options } => {
                let result = if let Some(existing) = &options.resume_session_id {
                    if !valid_id(existing) {
                        return Err(Error::Other(
                            "Invalid prepared continuation identity".into(),
                        ));
                    }
                    let result = self
                        .request(
                            "thread/resume",
                            json!({"threadId":existing,"cwd":options.cwd,"model":options.model,
                            "config":self.thread_config}),
                        )
                        .await?;
                    if result.pointer("/thread/id").and_then(Value::as_str)
                        != Some(existing.as_str())
                    {
                        return Ok(uncertain("thread_identity_mismatch"));
                    }
                    result
                } else {
                    self.request(
                        "thread/start",
                        json!({"cwd":options.cwd,"model":options.model,
                        "approvalPolicy":"never","sandbox":"read-only","ephemeral":false,
                        "config":self.thread_config}),
                    )
                    .await?
                };
                let id = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .filter(|s| valid_id(s))
                    .ok_or_else(|| Error::Other("Missing prepared thread identity".into()))?;
                Ok(CodexTurnObservation::Prepared {
                    thread_id: id.into(),
                })
            }
            CodexAppServerRequest::Inspect { binding, .. } => self.inspect(binding).await,
            CodexAppServerRequest::Run { options, binding } => {
                // Resume loads an already bound thread; the ONLY model dispatch is turn/start below.
                if self.prepared_thread.as_deref() != Some(binding.thread_id.as_str()) {
                    let resumed = self
                        .request(
                            "thread/resume",
                            json!({"threadId":binding.thread_id,
                    "cwd":options.cwd,"model":options.model,"config":self.thread_config}),
                        )
                        .await?;
                    if resumed.pointer("/thread/id").and_then(Value::as_str)
                        != Some(binding.thread_id.as_str())
                    {
                        return Ok(uncertain("thread_identity_mismatch"));
                    }
                }
                // No deduplication promise is inferred from clientUserMessageId. Run is never retried here.
                let result = self
                    .request(
                        "turn/start",
                        json!({"threadId":binding.thread_id,
                    "clientUserMessageId":binding.client_message_id,
                    "input":[{"type":"text","text":options.task,"text_elements":[]}],
                    "approvalPolicy":"never"}),
                    )
                    .await?;
                let turn_id = result
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .filter(|s| valid_id(s))
                    .ok_or_else(|| Error::Other("Missing started turn identity".into()))?
                    .to_string();
                if binding.turn_id.as_ref().is_some_and(|id| id != &turn_id) {
                    return Ok(uncertain("turn_identity_mismatch"));
                }
                loop {
                    let value = match self.notifications.pop_front() {
                        Some(v) => {
                            self.notification_bytes = self
                                .notification_bytes
                                .saturating_sub(serde_json::to_vec(&v)?.len());
                            v
                        }
                        None => self.read().await?,
                    };
                    if let Some(emit) = &self.progress {
                        project_notification(
                            &value,
                            binding,
                            &turn_id,
                            &mut self.parse_state,
                            &mut self.text_tracker,
                            emit.as_ref(),
                        );
                    }
                    if value.get("method").and_then(Value::as_str) == Some("turn/completed")
                        && value.pointer("/params/threadId").and_then(Value::as_str)
                            == Some(binding.thread_id.as_str())
                        && value.pointer("/params/turn/id").and_then(Value::as_str)
                            == Some(turn_id.as_str())
                    {
                        // Return only persisted exact-client correlation, not just a notification.
                        return self
                            .inspect(&CodexTurnBinding {
                                turn_id: Some(turn_id),
                                ..binding.clone()
                            })
                            .await;
                    }
                    if let Some(id) = value.get("id") {
                        self.send(json!({"id":id,"error":{"code":-32601,"message":"Unsupported server request"}})).await?;
                    }
                }
            }
        }
    }
}
fn classify(binding: &CodexTurnBinding, matches: &[Value]) -> CodexTurnObservation {
    if matches.len() != 1 {
        return uncertain(if matches.is_empty() {
            "correlated_turn_not_observed"
        } else {
            "duplicate_client_identity"
        });
    }
    let turn = &matches[0];
    let Some(id) = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| valid_id(s))
    else {
        return uncertain("missing_turn_identity");
    };
    if binding
        .turn_id
        .as_ref()
        .is_some_and(|expected| expected != id)
    {
        return uncertain("turn_identity_mismatch");
    }
    let Some(status @ ("completed" | "failed" | "interrupted" | "inProgress")) =
        turn.get("status").and_then(Value::as_str)
    else {
        return uncertain("unsupported_turn_status");
    };
    CodexTurnObservation::Observed {
        binding: CodexTurnBinding {
            turn_id: Some(id.into()),
            ..binding.clone()
        },
        status: status.into(),
        text: turn
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| {
                let text = items
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() { None } else { Some(text) }
            }),
    }
}
/// Same maintained stream projection as exec; provider payloads never name product scope.
fn project_notification(
    value: &Value,
    binding: &CodexTurnBinding,
    turn_id: &str,
    state: &mut super::parse::ParseState,
    tracker: &mut HashMap<String, String>,
    emit: &dyn Fn(StreamEvent),
) {
    if value.pointer("/params/threadId").and_then(Value::as_str) != Some(binding.thread_id.as_str())
        || value.pointer("/params/turnId").and_then(Value::as_str) != Some(turn_id)
    {
        return;
    }
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "item/agentMessage/delta" {
        if let (Some(id), Some(delta)) = (
            value.pointer("/params/itemId").and_then(Value::as_str),
            value.pointer("/params/delta").and_then(Value::as_str),
        ) {
            tracker.entry(id.into()).or_default().push_str(delta);
            emit(StreamEvent::TextDelta { text: delta.into() });
        }
        return;
    }
    if matches!(
        method,
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta"
    ) {
        if let Some(delta) = value.pointer("/params/delta").and_then(Value::as_str) {
            emit(StreamEvent::ThinkingDelta { text: delta.into() });
        }
        return;
    }
    let event = match method {
        "item/started" => "item.started",
        "item/completed" => "item.completed",
        _ => return,
    };
    let Some(mut item) = value.pointer("/params/item").cloned() else {
        return;
    };
    let kind = match item.get("type").and_then(Value::as_str) {
        Some("agentMessage") => "agent_message",
        Some("mcpToolCall") => "mcp_tool_call",
        Some("commandExecution") => "command_execution",
        _ => return,
    };
    item["type"] = json!(kind);
    // Reuse the installed exec parser's stable tool IDs, text delta suppression and tool result mapping.
    super::parse::parse_line(
        &json!({"type":event,"item":item}).to_string(),
        state,
        tracker,
        emit,
    );
}
/// Existing consumers can project these events through their current progress handler.
pub type CodexProgress = Arc<dyn Fn(StreamEvent) + Send + Sync>;
/// Caller ACK after persisting the exact prepared-thread to admitted-client identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexPreparedRun {
    pub binding: CodexTurnBinding,
    pub task: String,
}
pub type CodexAdmission = Box<
    dyn FnOnce(
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<CodexPreparedRun>> + Send>,
        > + Send,
>;
/// Prepare retains the same provider connection: zero-turn threads are not yet durable.
/// The callback MUST commit the binding to the caller's authoritative admission before returning.
pub async fn prepare_codex_turn(
    options: RunOptions,
    admit: CodexAdmission,
    cancel: CancellationToken,
) -> Result<CodexTurnObservation> {
    perform(
        CodexAppServerRequest::Prepare { options },
        cancel,
        Some(admit),
        None,
    )
    .await
}
/// Forward-only provider primitive. Does not persist bindings, settle product admission or resend.
/// Prepare requires `prepare_codex_turn` and its durable-admission callback.
pub async fn codex_app_server(
    request: CodexAppServerRequest,
    cancel: CancellationToken,
) -> Result<CodexTurnObservation> {
    if matches!(request, CodexAppServerRequest::Prepare { .. }) {
        return Err(Error::Other(
            "Prepare requires an admission callback".into(),
        ));
    }
    perform(request, cancel, None, None).await
}
/// Streaming variant of prepare; same durable ACK and exact-identity rules.
pub async fn prepare_codex_turn_with_progress(
    options: RunOptions,
    admit: CodexAdmission,
    cancel: CancellationToken,
    progress: CodexProgress,
) -> Result<CodexTurnObservation> {
    perform(
        CodexAppServerRequest::Prepare { options },
        cancel,
        Some(admit),
        Some(progress),
    )
    .await
}
/// Streaming variant for an already authorized, persisted thread intent.
pub async fn codex_app_server_with_progress(
    request: CodexAppServerRequest,
    cancel: CancellationToken,
    progress: CodexProgress,
) -> Result<CodexTurnObservation> {
    if matches!(request, CodexAppServerRequest::Prepare { .. }) {
        return Err(Error::Other(
            "Prepare requires an admission callback".into(),
        ));
    }
    perform(request, cancel, None, Some(progress)).await
}
async fn perform(
    request: CodexAppServerRequest,
    cancel: CancellationToken,
    admission: Option<CodexAdmission>,
    progress: Option<CodexProgress>,
) -> Result<CodexTurnObservation> {
    let (CodexAppServerRequest::Prepare { options }
    | CodexAppServerRequest::Run { options, .. }
    | CodexAppServerRequest::Inspect { options, .. }) = &request;
    if options.cli.is_some_and(|c| c != crate::CliName::Codex) {
        return Err(Error::Other("App-server capability is Codex only".into()));
    }
    if let CodexAppServerRequest::Run { binding, .. }
    | CodexAppServerRequest::Inspect { binding, .. } = &request
    {
        if !valid_id(&binding.thread_id)
            || !valid_id(&binding.client_message_id)
            || binding.turn_id.as_ref().is_some_and(|s| !valid_id(s))
        {
            return Err(Error::Other("Invalid provider turn binding".into()));
        }
    }
    if let CodexAppServerRequest::Run { options, binding } = &request {
        if options.task.trim().is_empty() || binding.turn_id.is_some() {
            return Err(Error::Other(
                "Run requires a new bound intent without a prior turn ID".into(),
            ));
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::Other(
            "Provider operation cancelled before start".into(),
        ));
    }
    let config = super::write_configs(options).await?;
    // app-server rejects --profile (including Codex's v2 profile files). Reuse
    // the complete private snapshot, but send its overrides through the official
    // thread/start and thread/resume config field. In particular MCP credentials
    // must not enter argv or a shared mutable config.toml. Keep `config` alive so
    // its temporary instruction file remains available throughout this operation.
    let thread_config = config
        .profile
        .as_ref()
        .map(|profile| -> Result<Value> {
            let content = std::fs::read_to_string(profile.path()).map_err(Error::Io)?;
            let table: toml::Table = toml::from_str(&content).map_err(|_| {
                Error::Other("Invalid private Codex invocation configuration".into())
            })?;
            serde_json::to_value(table)
                .map_err(|_| Error::Other("Cannot serialize Codex thread configuration".into()))
        })
        .transpose()?;
    let binary = match &options.executable_path {
        Some(s) => s.clone(),
        None => crate::discovery::discover_binary(crate::CliName::Codex)
            .await
            .ok_or(Error::NoCli)?,
    };
    let mut environment = options.env.clone().unwrap_or_default();
    if let Some(home) = &config.home {
        environment.insert("CODEX_HOME".into(), home.to_string_lossy().into());
    }
    let mut wrap = TokioCommandWrap::with_new(binary, |cmd| {
        cmd.arg("app-server").arg("--listen").arg("stdio://");
        if options.clear_env {
            cmd.env_clear();
        }
        cmd.envs(&environment)
            .current_dir(options.cwd.as_deref().unwrap_or("."))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
    });
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        wrap.wrap(process_wrap::tokio::CreationFlags(
            windows::Win32::System::Threading::CREATE_NO_WINDOW,
        ));
        wrap.wrap(process_wrap::tokio::JobObject);
    }
    let mut child = wrap
        .spawn()
        .map_err(|_| Error::Other("Cannot start Codex app-server".into()))?;
    let input = child
        .stdin()
        .take()
        .ok_or_else(|| Error::Other("Missing provider stdin".into()))?;
    let output = child
        .stdout()
        .take()
        .ok_or_else(|| Error::Other("Missing provider stdout".into()))?;
    let mut connection = Connection {
        child,
        input,
        output: BufReader::new(output),
        notifications: VecDeque::new(),
        notification_bytes: 0,
        initialized: false,
        prepared_thread: None,
        thread_config,
        next_id: 0,
        closed: false,
        progress,
        parse_state: super::parse::ParseState::default(),
        text_tracker: HashMap::new(),
        cap: options
            .max_output_bytes
            .unwrap_or(crate::DEFAULT_MAX_OUTPUT_BYTES)
            .clamp(1024, crate::DEFAULT_MAX_OUTPUT_BYTES),
    };
    let deadline =
        Duration::from_millis(options.total_timeout_ms.unwrap_or(60_000).clamp(1, 300_000));
    let outcome = tokio::select! {
        _ = cancel.cancelled() => Ok(uncertain("cancelled_requires_inspection")),
        result = tokio::time::timeout(deadline, async {
            if let Some(admit) = admission {
                let prepared = connection.operation(&request).await?;
                let CodexTurnObservation::Prepared { thread_id } = prepared else { return Ok(uncertain("prepare_failed")); };
                let ack = admit(thread_id.clone()).await?;
                if ack.binding.thread_id != thread_id || !valid_id(&ack.binding.client_message_id) || ack.binding.turn_id.is_some() || ack.task.trim().is_empty() {
                    return Err(Error::Other("Admission did not acknowledge prepared identity".into()));
                }
                connection.prepared_thread = Some(thread_id);
                let mut options = options.clone(); options.task = ack.task;
                connection.operation(&CodexAppServerRequest::Run { options, binding: ack.binding }).await
            } else { connection.operation(&request).await }
        }) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Ok(uncertain("provider_transport_or_protocol_failure")),
            Err(_) => Ok(uncertain("provider_deadline_requires_inspection")),
        },
    };
    let _ = Box::into_pin(connection.child.kill()).await;
    let _ = Box::into_pin(connection.child.wait()).await;
    connection.closed = true;
    outcome
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{collections::HashMap, os::unix::fs::PermissionsExt};
    const PROVIDER: &str = r#"#!/usr/bin/env python3
import sys,json,os
mode=os.environ.get('FAKE_MODE','completed')
log=os.environ['FAKE_LOG'];state=os.environ['FAKE_STATE']
if mode=='stdin-config':
 if '--profile' in sys.argv or any('fixture-secret' in a for a in sys.argv):sys.exit(2)

def emit(v): print(json.dumps(v),flush=True)
for line in sys.stdin:
 q=json.loads(line);m=q['method'];p=q.get('params',{})
 with open(log,'a') as f:f.write(m+'\n')
 if 'id' not in q:continue
 if mode=='unsupported':emit({'id':q['id'],'error':{'code':-32601}});continue
 if mode=='malformed':print('bad-json',flush=True);continue
 if mode=='stdin-config' and m in ['thread/start','thread/resume']:
  config=p.get('config',{})
  if config.get('mcp_servers',{}).get('cueframe',{}).get('http_headers',{}).get('Authorization')!='Bearer fixture-secret':sys.exit(3)
  prompt=config.get('model_instructions_file')
  if not prompt or open(prompt).read()!='Fixture instructions':sys.exit(4)
 if m=='initialize':r={}
 elif m in ['thread/start','thread/resume','thread/read']:r={'thread':{'id':'foreign' if mode=='foreign' else 'thread-1','turns':[]}}
 elif m=='turn/start':
  with open(state,'w') as f:json.dump({'id':'turn-1','items':[{'type':'userMessage','clientId':p['clientUserMessageId']},{'type':'agentMessage','id':'answer','text':'OK'}],'status':'completed'},f)
  if mode=='lost-ack':sys.exit(0)
  emit({'id':q['id'],'result':{'turn':{'id':'turn-1'}}})
  emit({'method':'item/agentMessage/delta','params':{'threadId':'thread-1','turnId':'turn-1','itemId':'answer','delta':'OK'}})
  emit({'method':'item/completed','params':{'threadId':'thread-1','turnId':'turn-1','item':{'type':'agentMessage','id':'answer','text':'OK'}}})
  emit({'method':'turn/completed','params':{'threadId':'thread-1','turn':{'id':'turn-1','status':'completed'}}});continue
 elif m=='thread/turns/list':
  t={'id':'turn-1','items':[{'type':'userMessage','clientId':'client-1'},{'type':'agentMessage','id':'answer','text':'OK'}],'status':mode if mode in ['completed','failed','interrupted','inProgress'] else 'completed'}
  if os.path.exists(state):t=json.load(open(state))
  if mode=='partial':t['itemsView']='summary'
  if mode=='compacted':t['items'].append({'type':'contextCompaction'})
  if mode=='unknown-status':t['status']='made-up'
  if mode=='wrong-client':t['items'][0]['clientId']='other'
  r={'data':[] if mode=='missing' else [t,t] if mode=='duplicate' else [t], 'nextCursor':'again' if mode=='cursor-loop' else None}
 else:emit({'id':q['id'],'error':{'code':-32601}});continue
 emit({'id':q['id'],'result':r})
"#;
    fn fixture(mode: &str) -> (tempfile::TempDir, RunOptions) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("provider");
        std::fs::write(&bin, PROVIDER).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = HashMap::from([
            ("FAKE_MODE".into(), mode.into()),
            (
                "FAKE_LOG".into(),
                dir.path().join("methods").to_string_lossy().into(),
            ),
            (
                "FAKE_STATE".into(),
                dir.path().join("state").to_string_lossy().into(),
            ),
        ]);
        let options = RunOptions {
            cli: Some(crate::CliName::Codex),
            task: "Say OK".into(),
            executable_path: Some(bin.to_string_lossy().into()),
            cwd: Some(dir.path().to_string_lossy().into()),
            env: Some(env),
            total_timeout_ms: Some(5_000),
            ..Default::default()
        };
        (dir, options)
    }
    fn binding() -> CodexTurnBinding {
        CodexTurnBinding {
            thread_id: "thread-1".into(),
            client_message_id: "client-1".into(),
            turn_id: None,
        }
    }
    fn ack(id: String) -> CodexPreparedRun {
        CodexPreparedRun {
            binding: CodexTurnBinding {
                thread_id: id,
                ..binding()
            },
            task: "Say OK".into(),
        }
    }
    #[tokio::test]
    async fn full_configuration_uses_private_stdin_for_start_and_resume() {
        for scenario in ["start", "resume", "run"] {
            let resume = scenario == "resume";
            let (dir, mut options) = fixture("stdin-config");
            let home = dir.path().join("owner");
            std::fs::create_dir_all(&home).unwrap();
            options
                .env
                .as_mut()
                .unwrap()
                .insert("CODEX_HOME".into(), home.to_string_lossy().into());
            options.system_prompt = Some("Fixture instructions".into());
            options.artifact_dir = Some(dir.path().to_string_lossy().into());
            options.mcp_servers = Some(HashMap::from([(
                "cueframe".into(),
                crate::McpServer {
                    url: Some("http://127.0.0.1:1/mcp".into()),
                    transport_type: Some(crate::McpTransport::Http),
                    headers: Some(HashMap::from([(
                        "Authorization".into(),
                        "Bearer fixture-secret".into(),
                    )])),
                    ..Default::default()
                },
            )]));
            if resume {
                options.resume_session_id = Some("thread-1".into());
            }
            let request = if scenario == "run" {
                CodexAppServerRequest::Run {
                    options,
                    binding: binding(),
                }
            } else {
                CodexAppServerRequest::Prepare { options }
            };
            let result = perform(request, CancellationToken::new(), None, None)
                .await
                .unwrap();
            if scenario == "run" {
                assert!(
                    matches!(result, CodexTurnObservation::Observed { .. }),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, CodexTurnObservation::Prepared { .. }),
                    "{result:?}"
                );
            }
            let methods = std::fs::read_to_string(dir.path().join("methods")).unwrap();
            assert!(methods.contains(if resume || scenario == "run" {
                "thread/resume"
            } else {
                "thread/start"
            }));
            assert_eq!(
                methods.lines().filter(|m| *m == "turn/start").count(),
                usize::from(scenario == "run")
            );
            assert!(!home.join("config.toml").exists());
            assert!(!home.join(".cli-agents-runtime/config.toml").exists());
        }
    }
    #[tokio::test]
    async fn prepare_requires_durable_ack_before_the_only_turn_dispatch() {
        let (dir, options) = fixture("completed");
        let log = dir.path().join("methods");
        let result = prepare_codex_turn(
            options,
            Box::new(move |id| {
                Box::pin(async move {
                    let methods = std::fs::read_to_string(log).unwrap();
                    assert!(methods.contains("thread/start"));
                    assert!(!methods.contains("turn/start"));
                    Ok(ack(id))
                })
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(result,CodexTurnObservation::Observed { status,.. } if status=="completed")
        );
        let log = std::fs::read_to_string(dir.path().join("methods")).unwrap();
        assert_eq!(log.lines().filter(|m| *m == "turn/start").count(), 1);
        assert!(!log.contains("thread/resume"));
    }
    #[tokio::test]
    async fn failed_admission_ack_does_not_dispatch() {
        let (dir, options) = fixture("completed");
        let result = prepare_codex_turn(
            options,
            Box::new(|_| {
                Box::pin(async { Err(Error::Other("Lost durable admission ACK".into())) })
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result, CodexTurnObservation::Uncertain { .. }));
        assert!(
            !std::fs::read_to_string(dir.path().join("methods"))
                .unwrap()
                .contains("turn/start")
        );
    }
    #[tokio::test]
    async fn wrong_prepared_thread_ack_does_not_dispatch() {
        let (dir, options) = fixture("completed");
        let result = prepare_codex_turn(
            options,
            Box::new(|_| Box::pin(async { Ok(ack("other".into())) })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result, CodexTurnObservation::Uncertain { .. }));
        assert!(
            !std::fs::read_to_string(dir.path().join("methods"))
                .unwrap()
                .contains("turn/start")
        );
    }
    #[tokio::test]
    async fn real_transport_lost_start_ack_recovers_exact_persisted_turn_without_redispatch() {
        let (dir, options) = fixture("lost-ack");
        let result = prepare_codex_turn(
            options.clone(),
            Box::new(|id| Box::pin(async move { Ok(ack(id)) })),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result, CodexTurnObservation::Uncertain { .. }));
        let recovered = codex_app_server(
            CodexAppServerRequest::Inspect {
                options,
                binding: binding(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(recovered,CodexTurnObservation::Observed { status,binding,.. } if status=="completed" && binding.turn_id.as_deref()==Some("turn-1"))
        );
        let log = std::fs::read_to_string(dir.path().join("methods")).unwrap();
        assert_eq!(log.lines().filter(|m| *m == "turn/start").count(), 1);
    }
    #[tokio::test]
    async fn inspection_is_read_only_and_preserves_all_native_statuses() {
        for status in ["completed", "failed", "interrupted", "inProgress"] {
            let (dir, options) = fixture(status);
            let result = codex_app_server(
                CodexAppServerRequest::Inspect {
                    options,
                    binding: binding(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(matches!(result,CodexTurnObservation::Observed { status:s,.. } if s==status));
            let log = std::fs::read_to_string(dir.path().join("methods")).unwrap();
            assert!(
                !log.contains("turn/start")
                    && !log.contains("thread/resume")
                    && !log.contains("thread/start")
            );
        }
    }
    #[tokio::test]
    async fn ambiguous_or_unsupported_history_is_never_authoritative_absence() {
        for mode in [
            "missing",
            "duplicate",
            "partial",
            "compacted",
            "unknown-status",
            "wrong-client",
            "foreign",
            "cursor-loop",
            "unsupported",
            "malformed",
        ] {
            let (_dir, options) = fixture(mode);
            let result = codex_app_server(
                CodexAppServerRequest::Inspect {
                    options,
                    binding: binding(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(result, CodexTurnObservation::Uncertain { .. }),
                "{mode}"
            );
        }
    }
    #[tokio::test]
    async fn exact_turn_mismatch_and_prestart_cancel_are_fenced() {
        let (dir, options) = fixture("completed");
        let mut bound = binding();
        bound.turn_id = Some("other".into());
        let result = codex_app_server(
            CodexAppServerRequest::Inspect {
                options: options.clone(),
                binding: bound,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result, CodexTurnObservation::Uncertain { .. }));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let before = std::fs::read_to_string(dir.path().join("methods")).unwrap();
        assert!(
            codex_app_server(
                CodexAppServerRequest::Run {
                    options,
                    binding: binding()
                },
                cancel
            )
            .await
            .is_err()
        );
        assert_eq!(
            before,
            std::fs::read_to_string(dir.path().join("methods")).unwrap()
        );
    }
    #[test]
    fn progress_reuses_existing_parser_and_refuses_foreign_thread_or_turn() {
        let events = std::sync::Mutex::new(Vec::new());
        let emit = |event| events.lock().unwrap().push(event);
        let mut state = super::super::parse::ParseState::default();
        let mut tracker = HashMap::new();
        let base = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"answer","delta":"Hello "}});
        let mut foreign = base.clone();
        foreign["params"]["threadId"] = json!("foreign");
        project_notification(
            &foreign,
            &binding(),
            "turn-1",
            &mut state,
            &mut tracker,
            &emit,
        );
        foreign["params"]["threadId"] = json!("thread-1");
        foreign["params"]["turnId"] = json!("other");
        project_notification(
            &foreign,
            &binding(),
            "turn-1",
            &mut state,
            &mut tracker,
            &emit,
        );
        assert!(events.lock().unwrap().is_empty());
        project_notification(&base, &binding(), "turn-1", &mut state, &mut tracker, &emit);
        for (method, item) in [
            (
                "item/completed",
                json!({"type":"agentMessage","id":"answer","text":"Hello world"}),
            ),
            (
                "item/started",
                json!({"type":"mcpToolCall","id":"tool-1","server":"cueframe","tool":"get_context","arguments":{"projectId":"owned"}}),
            ),
            (
                "item/completed",
                json!({"type":"mcpToolCall","id":"tool-1","status":"completed"}),
            ),
        ] {
            project_notification(
                &json!({"method":method,"params":{"threadId":"thread-1","turnId":"turn-1","item":item}}),
                &binding(),
                "turn-1",
                &mut state,
                &mut tracker,
                &emit,
            );
        }
        let events = events.into_inner().unwrap();
        let text = events
            .iter()
            .filter_map(|e| {
                if let StreamEvent::TextDelta { text } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(text, "Hello world");
        assert!(events.iter().any(|e|matches!(e,StreamEvent::ToolStart{tool_name,tool_id,..} if tool_name=="cueframe:get_context"&&tool_id=="tool-1")));
        assert!(events.iter().any(
            |e| matches!(e,StreamEvent::ToolEnd{tool_id,success:true,..} if tool_id=="tool-1")
        ));
    }
    #[tokio::test]
    async fn prepared_transport_streams_scoped_text_once_and_retains_terminal_text() {
        let (_dir, options) = fixture("completed");
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = events.clone();
        let result = prepare_codex_turn_with_progress(
            options,
            Box::new(|id| Box::pin(async move { Ok(ack(id)) })),
            CancellationToken::new(),
            Arc::new(move |e| capture.lock().unwrap().push(e)),
        )
        .await
        .unwrap();
        assert!(matches!(result,CodexTurnObservation::Observed{text:Some(text),..} if text=="OK"));
        let text = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| {
                if let StreamEvent::TextDelta { text } = e {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(text, "OK");
    }
    #[tokio::test]
    async fn prepare_existing_conversation_keeps_thread_before_admission_without_a_new_thread() {
        let (dir, mut options) = fixture("completed");
        options.resume_session_id = Some("thread-1".into());
        let log = dir.path().join("methods");
        let result = prepare_codex_turn(
            options,
            Box::new(move |id| {
                Box::pin(async move {
                    let methods = std::fs::read_to_string(log).unwrap();
                    assert!(methods.contains("thread/resume"));
                    assert!(!methods.contains("thread/start"));
                    assert!(!methods.contains("turn/start"));
                    Ok(ack(id))
                })
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(result,CodexTurnObservation::Observed{status,..} if status=="completed"));
    }
}
