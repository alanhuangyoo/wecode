# Architecture

WeCode keeps the runtime deliberately small and separates model access from repository mutation:

```text
CLI / benchmark manifest -> project instruction discovery
        |
        v
Agent loop -> Context window -> Turn decoder -> Model trait -> Provider protocol -> retrying HTTP
        |              |            |
        v              v            v
Tool-turn ledger -> Tool registry -> Executor
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

- One model call produces one or more native tool calls, a text fallback action, or a normal final
  response. `TurnDecoder` owns this normalization, provider stop reasons, loop detection, and
  bounded protocol recovery.
- Native mode trusts only provider-structured function calls. Plain assistant text is completion,
  even when it resembles the legacy JSON action syntax.
- `ToolTurnLedger` records an assistant tool batch and binds every result to the original provider
  call ID as execution completes. Interrupted and resumed turns receive explicit synthetic error
  results before the next provider request, so history never contains dangling calls.
- Provider `max_tokens`, refusal, safety, tool-use, and normal end-turn reasons are normalized.
  Truncated tool arguments are never executed.
- The registry allows up to eight parallel-capable calls to execute concurrently even when their
  tool kinds differ. Shell calls are individually classified and authorized during sequential
  preflight, then all results enter history in provider call order. Patch, control, and finish
  actions remain exclusive.
- Shell processes use the detected user shell with login semantics, run from the current working
  directory, and do not retain hidden state between calls.
- Provider implementations cannot mutate the repository directly.
- Provider prompt caching remains available. The local exact-response cache is off by default
  because commands, remote systems, and other external state may change between identical prompts.
- Context compaction follows Pi's structural cut and model-summary pipeline. It preserves the
  original task as a stable cache prefix, retains recent complete tool exchanges, and updates the
  prior structured checkpoint instead of recursively nesting summaries.
- Project instructions are ordered, content-deduplicated, and bounded before entering context.
  Compatible OpenCode, Claude Code, and Codex user instruction files are loaded before WeCode's
  own user rules, so the native WeCode layer retains precedence.
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
  the agent. The TUI presents a selection overlay for allow-once, a visible session rule, or deny.
  Decisions are paired by request ID, and dropped reviewers resolve to denial instead of hanging.
- Interactive plans are model-visible, locally validated, restored from conversation actions, and
  rendered in a fixed TUI panel. Question requests use a separate request/response channel and
  temporarily turn the composer into a numbered-choice or free-form answer box.
- Tool profiles are explicit. Chat, `run`, and `bench` use the native interactive profile, then
  filter tool schemas against attached runtime capabilities. Read-only review and subagent roles
  use smaller dedicated profiles.
- Review runs in an isolated five-tool read-only profile with a dedicated prompt and cache
  namespace. Structured findings are normalized and checked against changed line ranges before
  being recorded into the main session.
- Runtime trajectories and caches are stored outside the target repository unless explicitly
  redirected.

## Design sources

WeCode uses a small internal protocol, but its subsystem boundaries follow mature open-source
implementations:

- Pi supplies the nested turn loop, capability-based batch execution, and model-based compaction
  pipeline: assistant response, sequential preflight, ordered tool results, steering, structural
  history cut, and iterative checkpoint summary.
- OpenCode supplies explicit tool-call lifecycle state, capability-based tool registration,
  uncertainty-first investigation guidance, compatible global instruction discovery, and a
  repeated-call guard.
- Codex supplies model-specific prompt profiles, autonomous persistence rules, explicit reasoning
  effort, separation between runtime policy and provider protocol, user-shell execution, and scoped
  approval presentation.
- Grok Build supplies conversation integrity repair, resumable tool-call/result pairing, and
  trajectory-oriented runtime metrics.

Small adapted components remain local Rust modules so WeCode does not pull entire agent
applications into one binary. Their source and license are recorded in `THIRD_PARTY_NOTICES.md`.
No subsystem uses task-keyword routing.

## Agent loop

The interactive agent starts from the user's request and runtime environment. The working directory
is context and the default file root, not a declaration that every request is a repository task.
The model chooses from capabilities available in that runtime:

1. `read_file`, `list_files`, `glob`, and `grep` inspect the workspace through bounded native handlers.
   Independent calls can share a model step and run concurrently.
2. `shell` runs terminal, system, remote, build, test, and version-control commands from the current
   working directory.
3. `apply_patch` performs a bounded, path-checked file change.
4. Chat may expose `request_user_input` when an interactive broker exists.
5. Chat may expose `search_tools` to progressively load attached deferred capabilities.

`update_plan` is progressively discoverable. Control actions cannot be batched with repository
reads or mutations. A dropped question reviewer resolves safely instead of leaving the agent hung.
When the model returns normal assistant text, the turn decoder treats it as completion; optional
verification can reopen the run if its command fails.

The loop rejects a third identical tool turn with a model-visible nudge and stops after five
unchanged attempts. This guard compares structured action signatures rather than task text.

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
Approval requests carry both an exact action fingerprint and a visible reusable scope. Session
rules use command arity, SSH host, workspace patch, or MCP tool identity as appropriate. Destructive
and arbitrary-code commands stay exact even when the user chooses session approval. Grants remain
in memory and are never silently persisted to a repository.

Machine-oriented paths never block on a terminal prompt. JSONL and benchmark runs deny
approval-required actions when no reviewer is attached. The executor's dangerous-command denylist
remains a separate final gate and can be disabled explicitly only for an outer benchmark sandbox.

## Provider layer

All provider protocols normalize into one internal message, tool, response, stop-reason, and usage
model. This keeps the agent loop independent of HTTP wire formats and allows provider-specific
prompt caching without changing runtime behavior. OpenAI Chat Completions, OpenAI Responses,
Anthropic Messages, and Gemini `streamGenerateContent` SSE events are reconstructed into the same
completed response before the runtime decides whether to execute tools, recover, or complete.
OpenAI reasoning effort is a model capability: Responses receives `reasoning.effort`, while
compatible Chat Completions receives `reasoning_effort`. Providers that do not expose the capability
leave it unset.

Transient connection failures, HTTP 408/409/425/429/529 responses, and provider 5xx responses use
bounded exponential backoff with jitter. `Retry-After`, `retry-after-ms`, and `x-should-retry`
headers take precedence. Streams are retried only before the first event; once output starts, WeCode
will not replay the request and duplicate model output or tool calls.

## Output and replay

Each run can write a final Git patch, a structured result, and a JSONL trajectory. Results include
provider usage plus harness metrics for model turns, tool counts, recovery turns, loop nudges,
history repairs, and finish attempts. The manifest runner can assert required and forbidden tools
and a recovery budget. Exact model requests can be replayed only when the user opts into the local
response cache; verification and tool execution still run against current state.
