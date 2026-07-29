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
- Runtime trajectories and caches are stored outside the target repository unless explicitly
  redirected.

## Agent loop

The agent starts from a task and workspace description, then repeatedly requests one of three
actions:

1. `shell` inspects the workspace or runs build and test commands.
2. `apply_patch` performs a bounded, path-checked file change.
3. `finish` returns the final summary after optional verification succeeds.

Every turn emits typed events. Human mode renders compact progress in the terminal; JSONL mode
allows an external harness to record and analyze the same execution.

## Provider layer

All provider protocols normalize into one internal message, tool, response, and usage model. This
keeps the agent loop independent of HTTP wire formats and allows provider-specific prompt caching
without changing repository behavior.

## Output and replay

Each run can write a final Git patch, a structured result, and a JSONL trajectory. Exact model
requests can be replayed from disk cache, while verification and repository commands still execute
against the current workspace.
