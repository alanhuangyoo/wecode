# Architecture

WeCode keeps the runtime deliberately small and separates model access from repository mutation:

```text
CLI / benchmark manifest -> project instruction discovery
        |
        v
Agent loop -> Context window -> Model trait -> Provider protocol -> HTTP
        |                           |
        v                           v
Executor -> isolated process      exact-response cache
        |
        v
Git patch + JSONL trajectory

Interactive shell -> append-only session log -> resume / follow-up context
```

## Runtime boundaries

- One model call produces one native tool call or one text fallback action.
- Tool actions execute serially, so repository state changes are deterministic.
- Shell processes are independent and do not retain hidden session state.
- Provider implementations cannot mutate the repository directly.
- Model responses may be cached; command results and filesystem reads are never cached across
  repository mutations.
- Context compaction is local and deterministic and does not require another model call.
- Project instructions are ordered, content-deduplicated, and bounded before entering context.
- Interactive conversations use an append-only JSONL session log and a stable provider cache key.
- Interactive model calls can stream normalized text/reasoning deltas into the terminal renderer.
- Each active run owns a cancellation token. Cancelling drops in-flight HTTP or shell futures;
  patch application remains an atomic boundary and already-applied changes are preserved.
- Runtime trajectories and caches are stored outside the target repository unless explicitly
  redirected.

## Agent loop

The agent starts from a task and workspace description, then repeatedly requests one of three
actions:

1. `shell` inspects the workspace or runs build and test commands.
2. `apply_patch` performs a bounded, path-checked file change.
3. `finish` returns the final summary after optional verification succeeds.

Every turn emits typed events. Human mode opts into model deltas and renders compact live progress
in the terminal. JSONL and benchmark sinks do not request deltas, so they retain the buffered
provider path and stable machine-readable event stream.

## Provider layer

All provider protocols normalize into one internal message, tool, response, and usage model. This
keeps the agent loop independent of HTTP wire formats and allows provider-specific prompt caching
without changing repository behavior. OpenAI Chat Completions, OpenAI Responses, Anthropic
Messages, and Gemini `streamGenerateContent` SSE events are reconstructed into the same completed
response before the agent parses an action or writes to the exact-response cache.

## Output and replay

Each run can write a final Git patch, a structured result, and a JSONL trajectory. Exact model
requests can be replayed from disk cache, while verification and repository commands still execute
against the current workspace.
