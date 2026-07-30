# Benchmarking

## Recommended progression

1. Use a private 20-50 task regression set while changing prompts or control flow.
2. Run a small SWE-bench Lite or Verified subset in disposable containers.
3. Run the full target split only after cost, timeout, and patch collection are stable.
4. Use Terminal-Bench when evaluating general terminal competence rather than issue resolution.

Report at least:

- benchmark name and exact version
- model and provider
- `wecode` commit
- maximum steps and command timeout
- cache mode
- success rate
- mean and median input/output/cache tokens
- wall time and provider cost

Do not compare a warm exact-response replay against a cold benchmark run.

## SWE-bench integration

SWE-bench owns repository checkout, image setup, and grading. Within each prepared instance:

```bash
printf '%s' "$SWE_TASK" | wecode run \
  -C /workspace/repo \
  --provider "$PROVIDER" \
  --model "$MODEL" \
  --max-steps 40 \
  --unsafe-local \
  --output jsonl \
  --patch-out /results/model.patch \
  --result-out /results/run.json
```

The harness should copy `model.patch`, `run.json`, and the external trajectory directory out of the
container. Do not configure `--verify` to run hidden grader tests. Let the agent run repository tests,
and let SWE-bench grade the final patch separately.

`wecode bench` always disables the exact-response cache so a prior model response cannot satisfy a
new evaluation run. `wecode run` honors `--cache-mode`; use `off` for a clean external benchmark.
Provider-side prompt caching remains available and is reported separately.

Independent native read tools can execute concurrently inside one model step. Their observations
are returned in provider call order and share one hard output budget, so trajectories remain
deterministic and bounded. Repository mutations never execute in a parallel batch.

`run`, `bench`, and chat share the same native interactive prompt. The harness filters schemas to
actual runtime capabilities: autonomous runs expose repository read/search, shell, and patch tools;
chat adds question and deferred-tool discovery only when the corresponding brokers are attached.

## Terminal-Bench integration

Run one task per container and pass the task instruction on stdin. Terminal-Bench may require
commands rejected by the conservative local denylist, so use `--unsafe-local` only because the task
already runs inside an isolated benchmark container.

## Local manifest runner

The built-in `bench` command is intentionally sequential. It assumes each record points to an
already prepared workspace and never resets repositories. Parallel evaluation belongs in the outer
container scheduler, where isolation and GPU/API concurrency can be controlled correctly.

Each JSONL record accepts:

- `id`, `task`, and optional `workspace`, `verify`, and `max_steps`
- `required_tools`: tool kinds that must appear at least once
- `forbidden_tools`: tool kinds that must not appear
- `max_recoveries`: maximum format/protocol recovery turns
- `expected_success`: expected harness success, defaulting to `true`

The output includes a `passed` flag, individual checks, and harness metrics:
`model_turns`, compactions, `tool_calls`, per-tool counts, recoveries, loop nudges, history repairs,
and finish attempts.

Prompt and runtime changes should also run a small capability-routing suite with unrelated task
classes. At minimum, include a direct answer with no tools, a repository read, and a system command.
This catches prompts that force every request into a coding workflow without encoding
task-specific routing rules in the harness.

Run the repository smoke task with:

```bash
wecode bench examples/benchmark.jsonl --output /tmp/wecode-smoke.jsonl
```
