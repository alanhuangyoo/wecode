# WeCode

<p align="center">
  <strong>A lightweight, fast coding-agent CLI built in Rust.</strong>
</p>

<p align="center">
  <a href="README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/语言-简体中文-1f6feb"></a>
</p>

WeCode gives a model a focused loop for inspecting a repository, running commands, applying
patches, and verifying the result. It is designed for local development, automated repair tasks,
and coding-agent benchmarks without requiring a server, database, desktop application, or plugin
runtime.

## Why WeCode

- **Small and portable** — one Rust CLI with a narrow dependency graph and no Node.js or Python
  runtime.
- **Broad model support** — OpenAI Responses, OpenAI-compatible Chat Completions, Anthropic
  Messages, and Gemini `generateContent`.
- **Reliable tools** — native `shell`, `apply_patch`, and `finish` function calls, plus a JSON
  text-action fallback for gateways without tool-call support.
- **Purpose-built terminal UX** — a fast full-screen conversation timeline, persistent multiline
  composer, structured tool/output cards, live provider streaming, mid-run steering, queued
  follow-ups, cancellable runs, and slash commands.
- **Durable sessions** — conversations auto-save as inspectable JSONL, can be listed or resumed,
  and retain one stable provider cache identity across follow-up turns.
- **Repository-aware** — bounded global and hierarchical `AGENTS.md`, `CLAUDE.md`, and rules
  directories are loaded from repository root to the active workspace.
- **Cache efficient** — stable prompt prefixes, provider-side prompt-cache hints, and an on-disk
  exact-response cache for cheap, fast reruns.
- **Benchmark ready** — non-interactive execution, JSONL events and trajectories, final Git patch
  output, verification retries, and a JSONL manifest runner.
- **Predictable and safe by default** — bounded steps, timeouts, output truncation, API-key
  isolation, risk-classified approvals, session grants, and a conservative dangerous-command
  denylist.

## Supported providers

Built-in presets are available for OpenAI, Anthropic, Gemini, OpenRouter, DeepSeek, Groq, xAI,
Mistral, Ollama, LM Studio, and vLLM. Any compatible gateway can be selected with `--base-url`,
`--model`, and `--wire-api`.

```bash
wecode providers
```

## Install

Rust 1.85 or later is required.

```bash
git clone https://github.com/alanhuangyoo/wecode.git
cd wecode
cargo install --path .
```

After installation, start WeCode from the repository you want it to work on:

```bash
cd /path/to/repository
wecode
```

Running `wecode` with no subcommand opens the lightweight interactive session for the current
directory. To build without installing:

```bash
cargo build --release
./target/release/wecode --help
```

## Quick start

Run the guided setup once, then launch WeCode:

```bash
wecode setup
cd /path/to/repository
wecode
```

`wecode setup` asks for provider settings and reads the API key without echoing it. The key is
stored in `~/.wecode/credentials` with owner-only permissions and is never written into the
repository.

Alternatively, set the key expected by your provider and run a one-shot task:

```bash
export OPENAI_API_KEY="your-key"

wecode run -C /path/to/repository \
  --provider openai \
  --model gpt-5.4 \
  --verify "cargo test" \
  "Fix the failing tests and keep the public API unchanged."
```

Omit the task to read it from standard input:

```bash
printf '%s' "Find and fix the parser bug." | \
  wecode run -C /path/to/repository --provider openai
```

## Interactive session

The interactive interface keeps follow-up context while repository mutations remain visible to the
next turn. Its fixed header shows the active model, workspace, session, and protocol; the scrollable
timeline separates your messages, model activity, shell commands, patches, output, verification,
and final responses; and the bordered composer remains editable while the agent works.

```text
/new         Start a fresh saved session
/resume [id] Resume the latest or selected session
/sessions    List recent sessions for this workspace
/rename      Give the current session a title
/steer       Steer the active task at the next model boundary
/followup    Queue work for after the active task
/queue       Show pending steer and follow-up messages
/clear-queue Clear all pending messages
/cancel      Cancel the active request or command
/approve     Allow the pending action once
/approve-session
             Allow matching actions for this session
/deny        Deny the pending action with optional feedback
/status      Show model, workspace, cache, and context
/rules       Show loaded project instruction files
/config      Show the active config path
/history     Show the history file
/help        Show all commands
/quit        Exit
```

Sessions auto-save under `~/.wecode/sessions/chat/`. Resume outside the interactive interface with
`wecode resume [session-id]`, or inspect recent sessions with `wecode sessions`. Input history is
stored at `~/.wecode/history`. During a running turn, press `Ctrl-C` once to cancel the active
model request or shell command and return to the prompt; edits already applied are preserved.
The composer remains editable while the agent works: regular `Enter` steers the active task,
`Ctrl-J` inserts a newline, `Alt-Enter` queues a follow-up, `PageUp`/`PageDown` scroll the timeline,
and the explicit `/steer` and `/followup` commands provide the same behavior in terminals that do
not transmit modified Enter keys. Steering is delivered only at a safe model boundary; patch
application is never interrupted halfway through. Set `WECODE_TUI=0` to use the plain line-oriented
fallback in a terminal that does not support the full-screen interface.
The interactive renderer and provider streaming are isolated from
`wecode run --output jsonl` and `wecode bench`, so benchmark event output and agent execution do not
depend on terminal rendering or delta events.

## Project instructions

WeCode loads repository instructions automatically and injects them into both interactive and
benchmark tasks. Global files load first, followed by project files from repository root to the
active workspace, so deeper instructions take precedence.

Recognized sources include:

- `~/.wecode/AGENTS.md`, `~/.wecode/CLAUDE.md`, and `~/.wecode/rules/*.md`
- `AGENTS.md`, `CLAUDE.md`, and `CLAUDE.local.md` at each project level
- `.wecode/rules/*.md` and `.claude/rules/*.md` at each project level

Use `/rules` to inspect the exact files loaded for an interactive session. Each file and the total
instruction payload have hard size limits so repository context remains predictable.

## Configuration

Use the guided setup:

```bash
wecode setup
```

This writes `~/.wecode/config.toml` and, when a key is entered, the protected
`~/.wecode/credentials` file. `wecode init` remains available when only a starter configuration is
needed. WeCode keeps its response cache in `~/.wecode/cache/`, history in `~/.wecode/history`, and
trajectories in `~/.wecode/sessions/`. Set `WECODE_HOME` to move all locations, which is useful in
containers and benchmark workers.

You can also place `.wecode.toml` in a repository or pass `--config /path/to/config.toml`.
Repository configuration overrides the user configuration, and command-line options override the
loaded configuration.

```toml
[model]
provider = "openrouter"
family = "open-ai-compatible"
wire_api = "chat-completions"
model = "anthropic/claude-sonnet-4.6"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
max_output_tokens = 8192
prompt_cache = "auto"
send_prompt_cache_key = false
native_tools = true
streaming = true

[agent]
max_steps = 40
max_format_errors = 3
wall_time_limit_seconds = 1800
command_timeout_seconds = 120
command_output_bytes = 24000
context_max_tokens = 90000
context_keep_messages = 12
verify_retries = 2
deny_dangerous_commands = true
steering_mode = "one-at-a-time"
follow_up_mode = "one-at-a-time"
approval_policy = "on-request"
trajectory_directory = "/path/to/wecode/sessions"

[cache]
mode = "read-write"
max_megabytes = 2048
```

Provider-specific variables such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, and `GEMINI_API_KEY`
are supported. `WECODE_API_KEY` overrides the provider-specific variable. To avoid environment
variables, use an external key file:

```bash
chmod 600 /path/to/api-key
wecode run -C /path/to/repository \
  --provider openai \
  --api-key-file /path/to/api-key \
  "Fix the issue."
```

On Unix, key files must not be accessible by group or other users and must be located outside the
agent workspace.

## Approvals

Shell commands are classified as `read-only`, `workspace-write`, or `elevated`. Interactive
approval requests include the exact command or patch summary and support:

- `/approve` — allow this invocation once.
- `/approve-session` — remember the matching command or workspace-patch grant until WeCode exits.
- `/deny [reason]` — reject it and return the reason to the model so it can choose a safer approach.
- `Ctrl-C` — cancel the active run while leaving undelivered queue items intact.

Available policies are:

- `on-request` — default; ask for elevated operations such as network access, process control,
  publishing, or system package installation.
- `untrusted` — auto-approve only known read operations; ask before workspace writes and patches.
- `never` — never prompt. The dangerous-command denylist still applies unless `--unsafe-local` is
  also selected.

Override the policy with `--approval-policy`. Non-interactive JSONL runs never wait for a reviewer:
an action that requires approval is returned to the model as denied. `wecode bench` defaults to
`never` unless a policy is explicitly supplied, because benchmark workers are expected to provide
their own container or VM isolation.

## Custom OpenAI-compatible gateways

Choose Chat Completions for most third-party gateways:

```bash
export WECODE_API_KEY="your-gateway-key"

wecode run -C /path/to/repository \
  --provider openai \
  --base-url https://gateway.example/v1 \
  --model provider-model-id \
  --wire-api chat-completions \
  "Implement the requested change."
```

Native function tools are enabled by default. Add `--text-actions` if the gateway rejects tool
schemas or returns tool calls as plain text. Interactive provider streaming is enabled by default;
add `--no-stream` for gateways that only support buffered responses.

## Caching

The response cache key includes the provider configuration and exact request, so cached output is
only reused for an identical model call. Repository commands and file reads are never cached across
workspace mutations.

Available modes are:

- `read-write` — reuse existing entries and save new responses.
- `read-only` — deterministic replay without adding entries.
- `refresh` — bypass reads and replace entries.
- `off` — disable the exact-response cache for clean measurements.

```bash
wecode cache stats
wecode cache prune --max-megabytes 1024
wecode run --cache-mode off -C /path/to/repository "Run a cold evaluation."
```

## Benchmark usage

A manifest is a JSONL file with one prepared workspace per record:

```json
{"id":"task-001","task":"Fix the failing parser tests.","workspace":"/work/repo","verify":"cargo test","max_steps":40}
```

Run the manifest sequentially:

```bash
wecode bench tasks.jsonl \
  --provider openrouter \
  --model anthropic/claude-sonnet-4.6 \
  --output results.jsonl \
  --unsafe-local
```

For an external harness such as SWE-bench, run one isolated instance at a time and collect both
structured output and the final patch:

```bash
printf '%s' "$SWE_TASK" | wecode run \
  -C /workspace/repo \
  --provider "$PROVIDER" \
  --model "$MODEL" \
  --max-steps 40 \
  --output jsonl \
  --patch-out /results/model.patch \
  --result-out /results/run.json \
  --unsafe-local
```

JSONL and benchmark sinks intentionally use buffered model responses and emit no UI delta events,
even when `streaming = true`. Benchmark runs do not create an interactive input queue. This keeps
their prompts, trajectories, and execution deterministic and machine-readable.

WeCode never resets or cleans a worktree. The benchmark harness is responsible for repository
checkout, task isolation, and grading. See [docs/benchmarking.md](docs/benchmarking.md) for a
recommended evaluation workflow.

## Commands

```text
wecode run        Run one autonomous coding task
wecode chat       Start an interactive session
wecode resume     Resume the latest or a selected session
wecode sessions   List saved sessions for this workspace
wecode bench      Run a JSONL task manifest
wecode providers  List provider presets
wecode cache      Inspect or prune the response cache
wecode init       Write a starter configuration
```

Use `wecode <command> --help` for all options.

## Safety

Shell access is powerful. WeCode does not provide an OS sandbox. Run untrusted tasks and
`--unsafe-local` workloads inside a disposable container or virtual machine. Common model API-key
variables are removed from child-process environments.

## License

WeCode is licensed under the [MIT License](LICENSE). Required notices for bundled third-party
components are included in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
