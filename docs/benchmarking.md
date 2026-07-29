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

For a clean comparison, use `--cache-mode off`. For iteration on the harness itself, `read-write`
allows interrupted jobs to reuse identical provider responses. Always disclose the mode.

Independent native read tools can execute concurrently inside one model step. Their observations
are returned in provider call order and share one hard output budget, so trajectories remain
deterministic and bounded. Repository mutations never execute in a parallel batch.

## Terminal-Bench integration

Run one task per container and pass the task instruction on stdin. Terminal-Bench may require
commands rejected by the conservative local denylist, so use `--unsafe-local` only because the task
already runs inside an isolated benchmark container.

## Local manifest runner

The built-in `bench` command is intentionally sequential. It assumes each record points to an
already prepared workspace and never resets repositories. Parallel evaluation belongs in the outer
container scheduler, where isolation and GPU/API concurrency can be controlled correctly.
