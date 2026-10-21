# Opt-in Codex provider-turn correlation

This capability uses the installed official `codex app-server` protocol. The
existing `run`, `--config-stdin` and `codex exec` path remain the default. This is
an upstream local candidate; it is not integrated into a deployed CueFrame
runtime or a replacement product journal.

`prepare_codex_turn(options, admission_callback, cancellation)` retains one
provider connection. It calls `thread/start` without `turn/start`, then passes
the prepared thread ID to the trusted caller's asynchronous callback. The caller
must durably commit its deployment/credential owner/project/conversation/command
binding to that thread and a unique client message ID before acknowledging.
Only a successful, exact-thread acknowledgement dispatches the model turn.
Codex 0.162.1 does not retain a zero-turn thread after server exit, so prepare
and initial dispatch deliberately share that connection. A lost/rejected caller
admission acknowledgement does not dispatch a model turn.

The acknowledgement is `CodexPreparedRun { binding, task }`. `binding` contains
`threadId`, `clientMessageId` and optionally an already observed `turnId` for
inspection. Initial Run rejects a prior turn ID. The task cannot substitute for
identity, and old admissions without saved correlation cannot be retrospectively
certified by matching text, edits or session IDs.

`codex_app_server(Inspect { options, binding }, cancellation)` sends only
initialize, thread/read and paginated thread/turns/list requests. It never
starts/resumes a turn. The response is either an exact observed native status
(`completed`, `failed`, `interrupted`, `inProgress`) plus correlated turn ID, or
`uncertain`. Missing, duplicate, partial, compacted, malformed, unsupported,
transport-failed and bounded/truncated history remain uncertain. No result
from this primitive authorizes automatic redispatch. The client message ID is
correlation, not a provider deduplication promise.

A standalone opt-in `Run` supports an already persisted, caller-authorized
thread/client binding. It does not reserve product intent. The caller's existing
admission owner must prohibit a second dispatch of that intent. Product receipts
remain authoritative for product commits; a provider's terminal status does not
prove which product tools committed.

The CLI exposes `--codex-app-server-stdin`. Send one JSON line:

```json
{"operation":"prepare","options":{"cli":"codex","task":"","executablePath":"/absolute/path/to/codex","cwd":"/owned/workspace","totalTimeoutMs":1800000}}
```

It emits `{"state":"prepared","threadId":"..."}`. After committing the
trusted binding, send one second line (keep stdin open between lines):

```json
{"binding":{"threadId":"the-prepared-id","clientMessageId":"the-admitted-command-id"},"task":"the admitted task"}
```

For read-only inspection, use a new CLI process and one JSON line:

```json
{"operation":"inspect","options":{"cli":"codex","task":"","executablePath":"/absolute/path/to/codex","cwd":"/owned/workspace"},"binding":{"threadId":"the-stored-id","clientMessageId":"the-stored-command-id"}}
```

Do not accept caller-supplied paths, homes, credentials or bindings without
validating their owner/scope. Existing invocation-specific MCP/system-prompt
configuration and explicit child environment handling are reused; the snapshot
travels in the thread/start and thread/resume `config` field, never as a file.

Approval and sandbox follow `CodexOptions` with the same vocabulary as exec:
`approvalPolicy: "full-auto"` maps to approval `on-request` (and sandbox
`workspace-write` unless `sandboxMode` is set), `sandboxMode` passes through,
and `skipPermissions` without a policy selects `danger-full-access`. With none
of these, threads use approval `never` and sandbox `read-only`. The values are
sent on thread/start, thread/resume and turn/start. This adapter refuses every
server approval request, so `on-request` turns cannot obtain approvals.

Deadlines are split. Each control step (initialize, thread start/resume, the
caller's admission acknowledgement, and turn inspection) is bounded by
`providers.codex.appServerRequestTimeoutMs` (default 60 000 ms, capped at
300 000 ms). The model turn, from turn/start acceptance to turn/completed, is
bounded only by the caller's `totalTimeoutMs` (absolute, unlimited when unset)
and `idleTimeoutMs` (time between provider messages, default 300 000 ms; `0`
disables it). Any elapsed deadline yields
`uncertain: provider_deadline_requires_inspection`.

The operation has bounded messages/history and notification retention, uses
process groups/Windows job objects, and leaves timeout/cancellation uncertain
pending exact inspection. An `uncertain` observation carries a bounded (64 KiB)
`stderr` tail when the provider wrote one.

The optional `_with_progress` library variants reuse the maintained Codex
exec parser for scoped text/tool events. CLI `--json` emits those existing
StreamEvent lines between preparation and the final observation. Notifications
for another thread or turn are ignored, and completed text does not repeat
previous deltas. An exact observed turn may also carry its persisted assistant
`text` for terminal fallback or recovery. These are provider outputs, not product
commit evidence. Product admission/callback/schema integration, installed-artifact pinning,
and the deployed persisted-volume restart acceptance for CUE-808 are separate
remaining work. An ordinary successful plaintext probe does not close them.
