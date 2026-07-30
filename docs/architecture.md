# Architecture

WeCode keeps the runtime deliberately small and separates model access from repository mutation:

```text
CLI / benchmark manifest -> project instruction discovery
        |
        v
Agent loop -> Context window -> Model trait -> Provider protocol -> retrying HTTP
        |                           |
        v                           v
Tool registry -> Executor         exact-response cache
        |
        v
parallel reads / exclusive writes
        |
        v
Git patch + JSONL trajectory

Interactive shell -> append-only session log -> resume / checkpoint / fork
        |
        v
Composer -> steer queue (next model boundary) / follow-up queue (next turn)
        |
        v
Plan state + question broker + approval broker
```

## Runtime boundaries

- One model call produces one or more native tool calls, or one text fallback action.
- The registry allows up to eight independent repository reads to execute concurrently. Shell,
  patch, and finish actions are exclusive and execute one at a time, so mutations remain
  deterministic.
- Shell processes are independent and do not retain hidden session state.
- Provider implementations cannot mutate the repository directly.
- Model responses may be cached; command results and filesystem reads are never cached across
  repository mutations.
- Context compaction is local, deterministic, hard-bounded, and does not require another model
  call. It preserves the original task as a stable cache prefix and replaces the prior structured
  summary instead of recursively summarizing summaries.
- Project instructions are ordered, content-deduplicated, and bounded before entering context.
- Interactive conversations use an append-only JSONL session log and a stable provider cache key.
- Every task creates an automatic checkpoint. Manual checkpoints record the logical message state
  at that point; compaction is persisted as an appended snapshot record.
- Fork and rewind create a new child session from a checkpoint or the current state. They never
  truncate or rewrite the source log, so every branch remains resumable and auditable.
- Interactive model calls can stream normalized text/reasoning deltas into the terminal renderer.
- A dedicated input thread keeps line editing and history responsive while the Tokio agent loop
  runs. Rustyline's external-printer path redraws the composer around asynchronous agent events.
- Steering and follow-up queues are ordered and independent. Queue modes can deliver one message at
  a time or coalesce all currently pending messages while preserving their order.
- Each active run owns a cancellation token. Cancelling drops in-flight HTTP or shell futures;
  patch application remains an atomic boundary and already-applied changes are preserved.
- Approval requests use an in-process request/response channel rather than blocking stdin inside
  the agent. The composer remains responsive, decisions are paired by request ID, and dropped
  reviewers resolve to denial instead of hanging the run.
- Interactive plans are model-visible, locally validated, restored from conversation actions, and
  rendered in a fixed TUI panel. Question requests use a separate request/response channel and
  temporarily turn the composer into a numbered-choice or free-form answer box.
- Tool profiles are explicit. Interactive sessions add `update_plan` and `request_user_input`;
  `run --output jsonl` and `bench` retain the seven-tool coding profile, unchanged system prompt,
  and backward-compatible exact-cache namespace.
- Review runs in an isolated five-tool read-only profile with a dedicated prompt and cache
  namespace. Structured findings are normalized and checked against changed line ranges before
  being recorded into the main session.
- Runtime trajectories and caches are stored outside the target repository unless explicitly
  redirected.

## Agent loop

The agent starts from a task and workspace description, then repeatedly requests one of seven core
actions:

1. `read_file`, `list_files`, `glob`, and `grep` inspect the workspace through bounded native
   handlers. Independent calls can share a model step and run concurrently.
2. `shell` runs build, test, version-control, and repository-specific commands.
3. `apply_patch` performs a bounded, path-checked file change.
4. `finish` returns the final summary after optional verification succeeds.

Interactive sessions additionally expose `update_plan` and `request_user_input`. Both are
exclusive control actions: they cannot be batched with repository reads or mutations. A dropped
question reviewer resolves safely instead of leaving the agent hung. Once a plan exists, an
incomplete plan prevents `finish` and becomes a tool observation so the model can reconcile it.

Every turn emits typed events. Human mode opts into model deltas and renders compact live progress
in the terminal. JSONL and benchmark sinks do not request deltas, so they retain the buffered
provider path and stable machine-readable event stream. Benchmark runs also omit the input queue,
so interactive steering cannot change evaluation prompts or tool trajectories.

## Live input boundaries

The composer accepts input continuously:

1. A steering message is queued during sampling or tool execution.
2. The agent finishes the current atomic operation and injects ordered steering input before its
   next model request.
3. A pending steer prevents a just-produced `finish` action from ending the run prematurely.
4. Follow-up input stays separate and starts a new turn only after the active run finishes.
5. Cancellation preserves both applied workspace changes and undelivered queued input.

## Permission boundaries

The command classifier distinguishes known read operations, workspace mutations, and elevated
operations. Policy evaluation happens after the model action is recorded but before execution.
Denied actions become tool observations, allowing the model to recover without ending the turn.
Session grants are fingerprinted in memory and never silently persisted to a repository.

Machine-oriented paths never block on a terminal prompt. JSONL runs deny approval-required actions
when no reviewer is attached; the benchmark manifest runner defaults to `never` and relies on the
outer benchmark sandbox. The executor's dangerous-command denylist remains a separate final gate.

## Provider layer

All provider protocols normalize into one internal message, tool, response, and usage model. This
keeps the agent loop independent of HTTP wire formats and allows provider-specific prompt caching
without changing repository behavior. OpenAI Chat Completions, OpenAI Responses, Anthropic
Messages, and Gemini `streamGenerateContent` SSE events are reconstructed into the same completed
response before the agent parses an action or writes to the exact-response cache.

Transient connection failures, HTTP 408/409/425/429/529 responses, and provider 5xx responses use
bounded exponential backoff with jitter. `Retry-After`, `retry-after-ms`, and `x-should-retry`
headers take precedence. Streams are retried only before the first event; once output starts, WeCode
will not replay the request and duplicate model output or tool calls.

## Output and replay

Each run can write a final Git patch, a structured result, and a JSONL trajectory. Exact model
requests can be replayed from disk cache, while verification and repository commands still execute
against the current workspace.
