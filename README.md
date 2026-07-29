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
- **Reliable tools** — workspace-confined `read_file`, `list_files`, `glob`, and `grep`, plus
  `shell`, `apply_patch`, and `finish`; every action also has a JSON text fallback for gateways
  without native tool calls.
- **Purpose-built terminal UX** — a fast full-screen conversation timeline, persistent multiline
  composer, structured tool/output cards, live provider streaming, mid-run steering, queued
  follow-ups, cancellable runs, and slash commands.
- **Durable sessions** — conversations auto-save as inspectable JSONL, can be listed or resumed,
  and retain one stable provider cache identity across follow-up turns.
- **Repository-aware** — bounded global and hierarchical `AGENTS.md`, `CLAUDE.md`, and rules
  directories are loaded from repository root to the active workspace.
- **Semantic code intelligence** — installed language servers are detected automatically and
  started only when definitions, references, symbols, hover, call hierarchy, or diagnostics are
  needed.
- **Managed subagents** — delegate focused implementation, exploration, planning, or review work
  in isolated model contexts, run independent tasks concurrently, and continue completed agents
  without losing their conversation.
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
next turn. Its fixed header shows the active model, workspace, session, and protocol; a live plan
panel tracks multi-step work; the scrollable timeline separates your messages, model activity,
shell commands, patches, output, verification, questions, and final responses; and the bordered
composer remains editable while the agent works.

```text
/new         Start a fresh saved session
/resume [id] Resume the latest or selected session
/sessions    List recent sessions for this workspace
/rename      Give the current session a title
/checkpoint  Save a named checkpoint at the current conversation
/checkpoints List checkpoints in the current session
/fork        Fork from now or from a selected checkpoint
/rewind      Rewind safely by forking from an earlier checkpoint
/plan        Show the current task plan
/processes   Show managed background processes
/stop-process <id>
             Stop a managed background process tree
/agents      Show delegated subagents and their state
/stop-agent <id>
             Cancel a queued or running subagent
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
/mcp         Show connected MCP servers and tools
/lsp         Show detected, configured, and running language servers
/lsp-restart Stop active language servers and restart them lazily
/hooks       Show configured lifecycle hooks
/commands    Show reusable prompt commands
/skills      Show discovered Agent Skills
/skill:<name>
             Invoke a skill with optional arguments
/rules       Show loaded project instruction files
/config      Show the active config path
/history     Show the history file
/help        Show all commands
/quit        Exit
```

Sessions auto-save under `~/.wecode/sessions/chat/`. Resume outside the interactive interface with
`wecode resume [session-id]`, or inspect recent sessions with `wecode sessions`. Input history is
stored at `~/.wecode/history`. WeCode automatically creates a checkpoint before every task.
`/checkpoint [name]` adds a manual marker, `/fork [checkpoint]` branches from a marker (or the
current conversation), and `/rewind [checkpoint]` creates and switches to a fork of an earlier
state. Rewind never truncates or rewrites the original append-only session.

For substantial work, the agent can create and maintain a plan that remains visible above the
timeline; `/plan` shows it in the line-oriented fallback. When a choice materially changes the
result, the agent can pause and present two to four concrete options. Reply with an option number
or a free-form answer; for several questions, separate answers with semicolons. The composer changes
its title and key hints while an answer or approval is pending. If a plan exists, the harness asks
the model to mark every remaining step accurately before accepting `finish`.

During a running turn, press `Ctrl-C` once to cancel the active
model request or shell command and return to the prompt; edits already applied are preserved.
The composer remains editable while the agent works: regular `Enter` steers the active task,
`Ctrl-J` inserts a newline, `Alt-Enter` queues a follow-up, `PageUp`/`PageDown` scroll the timeline,
and the explicit `/steer` and `/followup` commands provide the same behavior in terminals that do
not transmit modified Enter keys. Steering is delivered only at a safe model boundary; patch
application is never interrupted halfway through. Set `WECODE_TUI=0` to use the plain line-oriented
fallback in a terminal that does not support the full-screen interface.
The interactive renderer and provider streaming are isolated from
`wecode run --output jsonl` and `wecode bench`, so benchmark event output and agent execution do not
depend on terminal rendering or delta events. Plan, question, skill, managed-process, LSP, and
subagent tools
are exposed only by interactive `wecode` sessions; machine-oriented runs retain the original
seven-tool profile and the same cache namespace.

Interactive agents can keep development servers, watchers, and long tests running without blocking
the conversation. `start_process` returns an ID, `process_status` reads bounded output incrementally
with a reusable cursor, `write_process` sends bounded stdin, and `stop_process` terminates the owned
process tree. Completion appears automatically in the timeline and is delivered to the model at its
next safe boundary, so it does not need to sleep or poll continuously. `/processes` and
`/stop-process <id>` remain usable while a task is active. All managed processes are stopped when
the session changes or WeCode exits.

For focused parallel work, the interactive agent can delegate to four built-in roles:
`general-purpose` may inspect, edit, and verify; `explore`, `plan`, and `review` are read-only.
Foreground delegation waits for its bounded result. Background delegation returns immediately,
shows progress in the timeline, and injects completion automatically at the next safe model
boundary. Completed agents keep their private conversation so the parent can send a follow-up
without starting over. `/agents` remains available while the parent is working, and
`/stop-agent <id>` requests cancellation.

Subagents share the same worktree. Run editing agents concurrently only for non-overlapping tasks;
there is no automatic Git worktree isolation yet. Concurrency, records, steps, runtime, output,
notifications, and waits all have hard limits. Subagents cannot recursively create more agents,
and all active child work is cancelled when the session changes or WeCode exits.

Long conversations are compacted locally into a deterministic, bounded summary that retains task
intent, inspected or edited paths, validation results, failures, and pending facts. Repeated
compaction replaces the previous summary instead of nesting summaries, while the original task
message remains a stable provider-cache prefix.

## Repository tools

Models can inspect code without spending turns constructing shell commands:

- `read_file` returns stable, numbered line ranges and an actionable next offset when more remains.
- `list_files` performs bounded directory traversal with deterministic sorting.
- `glob` finds paths using portable glob syntax.
- `grep` supports regex or literal search, case control, file globs, and context lines.

All four tools are confined to the active workspace, reject symlink escapes, skip Git metadata,
respect `.gitignore`, avoid binary and oversized files, and bound traversal, lines, matches, and
bytes. Their output is deterministic for benchmark trajectories. `shell` remains available for
builds, tests, version control, and repository-specific workflows.

Up to eight independent repository reads can share one model turn and execute concurrently, while
shell commands, patches, and finish actions remain exclusive. Provider requests retry transient
network, rate-limit, overload, and server failures with bounded jittered backoff and honor standard
retry headers without replaying a stream after output begins.

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
request_max_retries = 3
max_retry_delay_seconds = 60

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

[processes]
enabled = true
max_processes = 8
max_runtime_seconds = 3600
max_output_bytes = 131072

[lsp]
enabled = true
auto_detect = true
request_timeout_seconds = 30
max_message_bytes = 8388608
max_file_bytes = 8388608
max_output_bytes = 24000
diagnostic_settle_milliseconds = 350

# Optional: override auto-detection or add another server.
[lsp.servers.rust]
command = "rust-analyzer"
args = []
extensions = { ".rs" = "rust" }
startup_timeout_seconds = 15
enabled = true

[subagents]
enabled = true
max_agents = 16
max_concurrent = 4
max_steps = 20
max_runtime_seconds = 900
max_output_bytes = 32768
wait_timeout_seconds = 30

# Optional custom role. It replaces a built-in role with the same name.
[subagents.roles.security-review]
description = "Read-only security review"
prompt = "Inspect the delegated change for concrete security risks and report exact paths."
read_only = true
model = "provider-model-id"
max_steps = 12

[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/workspace"]
enabled = true
startup_timeout_seconds = 10
tool_timeout_seconds = 60
max_output_bytes = 65536

[skills]
enabled = true
discover_user = true
discover_project = true
compatibility_directories = true
paths = []
max_skills = 128
max_file_bytes = 131072

[commands]
enabled = true
discover_user = true
discover_project = true
compatibility_directories = true
paths = []
max_commands = 128
max_file_bytes = 65536

[hooks]
enabled = true
max_output_bytes = 32768

[[hooks.UserPromptSubmit]]
command = "./scripts/prompt-policy.sh"
command_windows = "powershell -File scripts/prompt-policy.ps1"
matcher = "deploy|release"
timeout_seconds = 10
async = false
fail_closed = true
status_message = "prompt policy"
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

The `[processes]` limits apply only to interactive managed processes. Output is retained in a
bounded ring buffer; status reads return `next_cursor`, report evicted bytes, and never inject
unbounded logs into model context. The provider API-key environment variable is removed from child
processes. This feature does not add tools to `wecode run` or `wecode bench`.

### Language-server intelligence

Interactive agents have one lightweight `lsp` tool for go-to-definition, references, hover,
document and workspace symbols, implementations, call hierarchy, and diagnostics. Matching files
are synchronized after successful patches, and bounded error/warning diagnostics are delivered to
the model at the next safe boundary. Use `/lsp` to inspect availability and `/lsp-restart` to clear
failed or stale server state.

With `auto_detect = true`, WeCode recognizes installed `rust-analyzer`,
`typescript-language-server`, `basedpyright`/`pyright`, `gopls`, `clangd`, `sourcekit-lsp`,
`lua-language-server`, `zls`, and `nil`. Discovery does not launch anything: a server starts only
after the agent queries or edits a matching source file. Protocol messages, source files, returned
results, diagnostic queues, and tracked documents all have hard bounds, and server process trees
are stopped when the session changes or exits. Common provider API-key variables are stripped from
language-server environments.

Configured servers under `[lsp.servers.<name>]` take precedence over auto-detected mappings. Keep
trusted commands in `~/.wecode/config.toml`. WeCode refuses to execute configured LSP commands from
an implicitly discovered project `.wecode.toml`; review it and pass it explicitly with `--config`
if it is trusted. LSP remains disabled in `wecode run` and `wecode bench`, preserving their exact
seven-tool prompt and cache path.

### Managed subagents

Subagents are available only in interactive sessions. `spawn_agent` supports foreground and
background execution and can launch several independent background tasks in one model turn.
`agent_status`, `send_agent`, `wait_agent`, and `stop_agent` provide the remaining lifecycle
operations. Background completion is pushed automatically, so normal operation does not require
polling.

The built-in `general-purpose` role receives the normal seven coding tools. The built-in
`explore`, `plan`, and `review` roles receive only `read_file`, `list_files`, `glob`, `grep`, and
`finish`. Configure additional roles under `[subagents.roles.<name>]`; a role may set a bounded
prompt, read-only policy, model override on the current provider, and step limit. WeCode refuses
custom roles from an implicitly discovered project `.wecode.toml`; review the file and pass it
explicitly with `--config`, or keep trusted role definitions in `~/.wecode/config.toml`.

Every child uses the current provider and existing response-cache implementation, while
credentials continue to come from the protected user configuration and are never copied into
prompts or child-process environments. Creating the manager is lazy: it makes no provider request
and launches no child until delegation is requested. Subagents are excluded from `wecode run` and
`wecode bench`, so benchmark tools, system prompt, cache namespace, and trajectory behavior remain
unchanged.

### MCP tools

Interactive sessions can connect to lightweight stdio
[Model Context Protocol](https://modelcontextprotocol.io/) servers configured under
`[mcp.servers.<name>]`. WeCode performs the initialize handshake, discovers paginated tools, and
registers them as deterministic `mcp__server__tool` functions. Use `/mcp` to inspect connection
errors and discovered tools.

Server startup, calls, protocol lines, stderr capture, tool count, and returned observations all
have hard bounds. Servers are killed when the session closes. Tools declaring the MCP
`readOnlyHint` run directly; other MCP tools use the normal approval UI because they may change
external state. Optional environment values can be added under
`[mcp.servers.<name>.env]`; keep secrets in the user-level `~/.wecode/config.toml`, never in a
repository config. To prevent repository checkout attacks, WeCode refuses to auto-start enabled
MCP commands from an implicitly discovered `.wecode.toml`; review the file and pass it explicitly
with `--config`, or move trusted MCP configuration to `~/.wecode/config.toml`.

MCP is deliberately disabled for `wecode run`, JSONL output, and `wecode bench`. Those paths retain
the original seven tools, system prompt, and cache namespace, so adding interactive extensions
cannot silently change benchmark results.

### Agent Skills

WeCode supports the portable `SKILL.md` format with progressive disclosure. At startup, only a
skill's validated name and description enter the system prompt. The model calls `load_skill` when
a task matches, then reads referenced files relative to the skill directory as needed. This keeps
normal prompts small while preserving complete workflows, scripts, references, and assets.

Skills are discovered deterministically from:

- User scope: `~/.wecode/skills/` and, when compatibility is enabled,
  `~/.agents/skills/`, `~/.claude/skills/`, and `~/.codex/skills/`.
- Project scope: the same hidden skill directories from the repository root through the current
  workspace.
- Explicit paths: additive entries in `skills.paths`; relative paths resolve from the workspace.

More specific project roots override user skills with the same name. Names follow the Agent Skills
lowercase/hyphen convention, descriptions and catalogs are bounded, invalid skills produce visible
diagnostics, and resource paths cannot escape the canonical skill directory. A skill with
`disable-model-invocation: true` remains available through `/skill:<name>` but is omitted from
automatic model discovery. `/skills` shows scope and visibility for the active catalog.

As with MCP, Skills are interactive-only by default. `wecode run` and `wecode bench` retain the
unchanged seven-tool benchmark profile.

### Prompt commands

Reusable Markdown prompts turn common workflows into slash commands without adding runtime weight
or changing the model tool set. Put `review.md` in `~/.wecode/commands/` or
`.wecode/commands/`, then invoke it as `/review`. WeCode also discovers compatible
Pi, Claude Code, and OpenCode prompt directories, including `~/.pi/agent/prompts/`,
`~/.claude/commands/`, `~/.config/opencode/{command,commands}/`, `.pi/prompts/`,
`.claude/commands/`, and `.opencode/{command,commands}/`, when compatibility is enabled.

An optional YAML frontmatter block supports `description` and `argument-hint`. Templates support
quoted arguments, `$1`, `$2`, `$@`, `$ARGUMENTS`, defaults such as `${1:-src}`, and slices such as
`${@:2}`. `/commands` shows the deterministic active catalog and its precedence scope. Built-in
commands cannot be shadowed, files and catalogs have hard limits, and external `commands.paths`
from an automatically loaded project config require explicit trust.

Prompt commands are expanded only in interactive chat. Benchmark prompts, tools, and cache
namespaces remain unchanged.

### Lifecycle hooks

WeCode runs bounded command hooks for `SessionStart`, `UserPromptSubmit`, `Stop`, and `SessionEnd`.
Each hook receives a small JSON object on stdin containing the event name, session ID, workspace,
provider, model, source, and only the event-specific prompt or stop reason. Known provider API-key
environment variables are removed from the child process.

Hooks support regular-expression matchers, platform-specific Windows commands, per-command
timeouts, hard output limits, visible status messages, fail-open or fail-closed behavior, and
non-blocking asynchronous notifications. Async hooks cannot be fail-closed because their result is
intentionally not awaited.

Exit code `2`, `{"continue":false}`, or `{"decision":"block"}` blocks a prompt or asks the agent
to continue instead of stopping. Successful JSON output may add bounded model context with
`additionalContext`; `suppressOutput` hides routine stdout from the timeline. Stop hooks are capped
at three continuation turns to prevent loops. `/hooks` shows the active event catalog.

Automatically loaded project config cannot execute hooks. Review a project `.wecode.toml` and pass
it explicitly with `--config`, or keep trusted hooks in `~/.wecode/config.toml`.

Hooks are interactive-only. `wecode run` and `wecode bench` may deserialize the shared config, but
they do not discover, construct, or execute hooks, and project Hook declarations do not alter the
benchmark tool registry or prompt path.

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
