# WeCode

<p align="center">
  <strong>轻量、快速的 Rust Coding Agent 命令行工具。</strong>
</p>

<p align="center">
  <a href="README.md"><img alt="English" src="https://img.shields.io/badge/Language-English-1f6feb"></a>
</p>

WeCode 为大模型提供一个专注的代码执行循环：检查仓库、运行命令、应用补丁并验证结果。
它适合本地开发、自动修复任务和 Coding Agent Benchmark，不需要服务器、数据库、桌面应用
或插件运行时。

## 核心优势

- **轻量且易分发**：单个 Rust CLI，依赖精简，不需要 Node.js 或 Python 运行时。
- **广泛兼容模型**：支持 OpenAI Responses、OpenAI 兼容的 Chat Completions、
  Anthropic Messages 和 Gemini `generateContent`。
- **工具调用可靠**：原生提供限制在工作区内的 `read_file`、`list_files`、`glob`、`grep`，
  以及 `shell`、`apply_patch`、`finish`；不支持原生工具调用的网关可用 JSON 文本动作协议兜底。
- **专门设计的终端交互**：快速的全屏对话时间线、常驻多行输入框、工具与输出卡片、模型实时
  流式状态、运行中修正方向、后续任务队列、可取消任务和斜杠命令。
- **持久化会话**：对话自动保存为可读的 JSONL，可列出和恢复；多轮对话使用稳定的服务端
  缓存标识，避免每轮破坏 Prompt Cache。
- **理解仓库规范**：从仓库根目录到当前工作区分层加载受限大小的 `AGENTS.md`、
  `CLAUDE.md` 和 rules 目录。
- **语义级代码理解**：自动发现已安装的语言服务器，仅在查询定义、引用、符号、Hover、
  调用层级或诊断时按需启动。
- **高缓存命中设计**：稳定的 Prompt 前缀、服务端 Prompt Cache 提示，以及本地精确响应缓存，
  让重复运行更快、更省 Token。
- **适合跑榜**：支持非交互运行、JSONL 事件与轨迹、最终 Git Patch、验证重试和 JSONL
  批量任务。
- **默认行为可预测**：限制最大步骤、执行时间和命令输出，并隔离 API Key、提供风险分级审批
  和会话授权、拦截明显危险命令。

## 支持的模型服务

内置 OpenAI、Anthropic、Gemini、OpenRouter、DeepSeek、Groq、xAI、Mistral、Ollama、
LM Studio 和 vLLM 预设。其他兼容网关可以通过 `--base-url`、`--model` 和 `--wire-api`
接入。

```bash
wecode providers
```

## 安装

需要 Rust 1.85 或更高版本。

```bash
git clone https://github.com/alanhuangyoo/wecode.git
cd wecode
cargo install --path .
```

安装后，进入希望 WeCode 操作的仓库，直接启动：

```bash
cd /path/to/repository
wecode
```

不带子命令运行 `wecode`，会为当前目录启动轻量交互会话。只构建、不安装：

```bash
cargo build --release
./target/release/wecode --help
```

## 快速开始

首次运行引导式配置，然后启动：

```bash
wecode setup
cd /path/to/repository
wecode
```

`wecode setup` 会引导配置模型服务，并以隐藏输入方式读取 API Key。密钥保存在权限仅限
当前用户的 `~/.wecode/credentials`，不会进入项目仓库。

也可以直接设置模型服务密钥，并执行一次性任务：

```bash
export OPENAI_API_KEY="your-key"

wecode run -C /path/to/repository \
  --provider openai \
  --model gpt-5.4 \
  --verify "cargo test" \
  "修复失败的测试，并保持公开 API 不变。"
```

也可以通过标准输入传入任务：

```bash
printf '%s' "定位并修复解析器错误。" | \
  wecode run -C /path/to/repository --provider openai
```

## 交互会话

交互模式会保留多轮上下文，下一轮也能看到前面已完成的修改。顶部固定显示当前模型、
工作区、会话和协议；实时计划面板会跟踪多步骤任务；中间的可滚动时间线会区分用户消息、
模型状态、Shell 命令、文件编辑、命令输出、验证、提问和最终回答；底部带边框的输入框在
Agent 工作时仍可继续编辑。

```text
/new         创建新的持久化会话
/resume [id] 恢复最近或指定会话
/sessions    列出当前工作区的最近会话
/rename      设置当前会话标题
/checkpoint  在当前对话位置保存命名检查点
/checkpoints 列出当前会话的检查点
/fork        从当前位置或指定检查点创建分支
/rewind      通过分支安全回到更早的检查点
/plan        查看当前任务计划
/processes   查看受管理的后台进程
/stop-process <id>
             停止一个后台进程树
/steer       在下一个模型边界修正当前任务
/followup    将任务排到当前任务结束之后
/queue       查看待处理的 steer 和 follow-up
/clear-queue 清空待处理消息
/cancel      取消当前模型请求或命令
/approve     仅允许当前待审批操作
/approve-session
             本会话允许相同操作
/deny        拒绝操作并可提供原因
/status      查看模型、工作区、缓存和上下文
/mcp         查看 MCP 服务器和工具状态
/lsp         查看已发现、已配置和运行中的语言服务器
/lsp-restart 停止语言服务器，并在下次需要时重新启动
/hooks       查看生命周期 Hooks
/commands    查看可复用 Prompt 命令
/skills      查看发现的 Agent Skills
/skill:<名称>
             调用 Skill 并可附带参数
/rules       查看已加载的项目规则
/config      查看当前配置文件
/history     查看输入历史文件
/help        查看所有命令
/quit        退出
```

会话自动保存在 `~/.wecode/sessions/chat/`。也可以在终端使用 `wecode resume [会话ID]`
恢复会话，或用 `wecode sessions` 查看最近会话。输入历史保存在 `~/.wecode/history`。
每个任务开始前，WeCode 都会自动创建检查点。`/checkpoint [名称]` 可添加手动检查点，
`/fork [检查点]` 会从检查点（不传参数时为当前位置）创建分支，`/rewind [检查点]`
会创建并切换到更早状态的分支。Rewind 永远不会截断或改写原始的 append-only 会话。

面对复杂任务时，Agent 可以创建并持续更新计划，计划会固定显示在时间线上方；普通行模式
可以使用 `/plan` 查看。当某个选择会实质影响结果时，Agent 可以暂停并给出两到四个具体
选项。输入选项编号或自由回答即可；多个问题使用分号分隔。等待回答或审批时，输入框标题、
占位提示和快捷键说明会自动切换。如果任务已经创建计划，Harness 会要求模型在 `finish`
前准确完成所有剩余步骤。

任务运行中按一次 `Ctrl-C` 会取消当前模型请求或 Shell 命令并返回输入框；已经应用的修改
会保留。Agent 工作时输入框仍然可编辑：普通 `Enter` 会 steer 当前任务，`Ctrl-J` 插入换行，
`Alt-Enter` 会把消息放入 follow-up 队列，`PageUp`/`PageDown` 可滚动时间线；不支持组合键的
终端可使用 `/steer` 和 `/followup`。Steer 只在安全的模型边界注入，不会在 Patch 应用到一半
时中断。不支持全屏界面的终端可设置 `WECODE_TUI=0` 使用普通行模式。交互渲染和模型流式事件与
`wecode run --output jsonl`、`wecode bench`
完全分离，因此 Benchmark 的事件输出和 Agent 执行不依赖终端 UI 或流式增量事件。
计划、提问、Skill、后台进程和 LSP 工具只在交互式 `wecode` 会话中暴露；机器执行仍使用
原来的七工具配置和同一缓存命名空间。

交互 Agent 可以让开发服务器、Watcher 或长时间测试在后台运行，同时继续对话。
`start_process` 会返回进程 ID；`process_status` 使用可复用游标增量读取有界输出；
`write_process` 可写入有大小限制的 stdin；`stop_process` 会结束整个受管理的进程树。
进程完成时会自动显示在时间线，并在下一个安全模型边界通知 Agent，无需反复 sleep 或轮询。
任务运行中也可使用 `/processes` 和 `/stop-process <id>`。切换会话或退出 WeCode 时，所有
受管理进程都会被回收。

长对话会在本地压缩为确定且有硬上限的结构化摘要，保留任务意图、检查或修改过的路径、
验证结果、失败信息和待处理事实。重复压缩会替换上一份摘要，不会产生“摘要套摘要”；
最初的任务消息保持不变，可作为稳定的 Provider 缓存前缀。

## 仓库工具

模型无需浪费轮次拼接 Shell 命令，就能直接检查代码：

- `read_file` 返回稳定的带行号区间，内容未读完时会给出下一次应使用的 offset。
- `list_files` 进行有界目录遍历并稳定排序。
- `glob` 使用跨平台 Glob 语法查找路径。
- `grep` 支持正则或字面量、大小写控制、文件 Glob 和上下文行。

这四个工具都限制在当前工作区内，会拒绝 symlink 逃逸、跳过 Git 元数据、遵循
`.gitignore`、避开二进制和超大文件，并限制遍历量、行数、匹配数和输出字节。输出顺序
确定，适合 Benchmark 轨迹复现。构建、测试、版本控制和仓库特有工作流仍可使用 `shell`。

最多八个相互独立的仓库读取可共用一个模型轮次并发执行；Shell、补丁和完成动作保持独占。
遇到临时网络故障、限流、过载或服务端错误时，会进行有界的抖动退避并遵循标准重试响应头；
流式输出一旦开始就不会重放请求，避免重复文本或工具调用。

## 项目规则

WeCode 会自动加载仓库指令，并同时应用于交互任务和 Benchmark 任务。全局规则最先加载，
随后按仓库根目录到当前工作区的顺序加载项目规则，因此更深层目录的指令优先级更高。

支持的来源包括：

- `~/.wecode/AGENTS.md`、`~/.wecode/CLAUDE.md` 和 `~/.wecode/rules/*.md`
- 项目各层级的 `AGENTS.md`、`CLAUDE.md` 和 `CLAUDE.local.md`
- 项目各层级的 `.wecode/rules/*.md` 和 `.claude/rules/*.md`

交互会话中可用 `/rules` 查看实际加载的文件。每个文件和规则总量都有硬限制，避免仓库上下文
无限增长。

## 配置

使用引导式配置：

```bash
wecode setup
```

该命令会写入 `~/.wecode/config.toml`；输入密钥时，还会生成受保护的
`~/.wecode/credentials`。如果只需要初始配置，仍可使用 `wecode init`。响应缓存位于
`~/.wecode/cache/`，输入历史位于 `~/.wecode/history`，运行轨迹位于
`~/.wecode/sessions/`。可以通过 `WECODE_HOME` 同时修改这些位置，便于容器和
Benchmark Worker 使用。

也可以在仓库根目录放置 `.wecode.toml`，或使用 `--config /path/to/config.toml`。
仓库配置会覆盖用户配置，命令行参数的优先级最高。

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

# 可选：覆盖自动发现映射，或添加其他服务器。
[lsp.servers.rust]
command = "rust-analyzer"
args = []
extensions = { ".rs" = "rust" }
startup_timeout_seconds = 15
enabled = true

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

支持 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`GEMINI_API_KEY` 等服务商变量。
`WECODE_API_KEY` 会覆盖服务商专用变量。也可以把密钥放在工作区外部文件中：

```bash
chmod 600 /path/to/api-key
wecode run -C /path/to/repository \
  --provider openai \
  --api-key-file /path/to/api-key \
  "修复这个问题。"
```

在 Unix 系统上，密钥文件不能允许用户组或其他用户读取，并且必须位于 Agent 工作区之外。

`[processes]` 限制只用于交互模式的受管理后台进程。输出保存在有硬上限的环形缓冲区中；
状态读取会返回 `next_cursor`、报告已经淘汰的字节，不会把无限日志注入模型上下文。子进程
不会继承模型服务商的 API Key 环境变量。该功能不会给 `wecode run` 或 `wecode bench`
增加任何工具。

### 语言服务器智能

交互 Agent 提供一个轻量 `lsp` 工具，支持跳转定义、查找引用、Hover、文档与工作区符号、
实现、调用层级和诊断。成功应用 Patch 后会同步匹配文件，并在下一个安全模型边界注入有硬
上限的错误和警告诊断。使用 `/lsp` 查看服务器状态，使用 `/lsp-restart` 清除失败或过期状态。

启用 `auto_detect = true` 后，WeCode 会识别已安装的 `rust-analyzer`、
`typescript-language-server`、`basedpyright`/`pyright`、`gopls`、`clangd`、
`sourcekit-lsp`、`lua-language-server`、`zls` 和 `nil`。发现过程不会启动任何进程；
只有 Agent 查询或编辑匹配的源码文件时才会按需启动。协议消息、源码文件、返回结果、诊断
队列和已跟踪文档都有硬上限；切换会话或退出时会回收整个语言服务器进程树。语言服务器也
不会继承常见模型服务商 API Key 环境变量。

`[lsp.servers.<名称>]` 下的配置会优先于自动发现映射。可信命令建议放在用户级
`~/.wecode/config.toml`。WeCode 不会执行隐式发现的项目 `.wecode.toml` 中配置的 LSP
命令；请先审查文件，确认可信后再用 `--config` 显式传入。LSP 不会进入 `wecode run` 和
`wecode bench`，因此它们仍保持完全相同的七工具 Prompt 和缓存路径。

### MCP 工具

交互会话可以连接配置在 `[mcp.servers.<名称>]` 下的轻量 stdio
[Model Context Protocol](https://modelcontextprotocol.io/) 服务器。WeCode 会完成初始化握手、
分页发现工具，并把它们注册为确定的 `mcp__服务器__工具` 函数。使用 `/mcp` 可以查看连接
错误和已发现的工具。

服务器启动、调用、协议行、stderr 捕获、工具数量和返回给模型的观察结果都有硬上限；
会话关闭时子进程会被回收。声明 MCP `readOnlyHint` 的工具可以直接执行，其他 MCP 工具
会进入现有审批界面，因为它们可能修改外部状态。可在 `[mcp.servers.<名称>.env]` 下添加
环境变量；密钥只应放在用户级 `~/.wecode/config.toml`，不要写进仓库配置。为防止恶意仓库
在进入目录时自动执行命令，WeCode 不会从隐式发现的 `.wecode.toml` 启动已启用的 MCP；
请先检查文件，再通过 `--config` 显式传入，或把可信 MCP 配置移到
`~/.wecode/config.toml`。

MCP 在 `wecode run`、JSONL 输出和 `wecode bench` 中默认完全禁用。这些路径继续使用原来的
七个工具、系统提示词和缓存命名空间，交互扩展不会悄悄改变 Benchmark 结果。

### Agent Skills

WeCode 支持可移植的 `SKILL.md` 格式和渐进披露。启动时只有通过校验的名称与描述进入系统
提示词；任务匹配后模型才调用 `load_skill`，并按需读取 Skill 目录中的相对引用文件。普通
请求不会携带完整说明，同时仍可使用完整工作流、脚本、参考资料和资源。

Skill 会按确定顺序从以下位置发现：

- 用户级：`~/.wecode/skills/`；启用兼容目录时，还包括 `~/.agents/skills/`、
  `~/.claude/skills/` 和 `~/.codex/skills/`。
- 项目级：从仓库根目录到当前工作区的同名隐藏 Skills 目录。
- 显式路径：`skills.paths` 中的附加路径；相对路径以当前工作区为基准。

更具体的项目 Skill 会覆盖同名用户 Skill。名称遵循 Agent Skills 的小写字母与连字符规范；
描述、目录和文件大小都有硬上限；无效 Skill 会显示诊断；资源路径不能逃逸规范化后的 Skill
目录。带 `disable-model-invocation: true` 的 Skill 不参与模型自动发现，但仍可通过
`/skill:<名称>` 显式调用。`/skills` 会显示当前目录中每个 Skill 的作用域和可见性。

与 MCP 一样，Skills 默认只在交互模式启用；`wecode run` 和 `wecode bench` 继续保持不变的
七工具 Benchmark 配置。

### Prompt 命令

可复用 Markdown Prompt 可以把常见工作流变成斜杠命令，不增加运行时负担，也不改变模型工具
集合。把 `review.md` 放进 `~/.wecode/commands/` 或 `.wecode/commands/`，即可通过
`/review` 调用。启用兼容目录时，还会发现 `~/.pi/agent/prompts/`、
`~/.claude/commands/`、`~/.config/opencode/{command,commands}/`、`.pi/prompts/`、
`.claude/commands/` 和 `.opencode/{command,commands}/` 等 Pi、Claude Code 与 OpenCode
Prompt 目录。

可选 YAML frontmatter 支持 `description` 和 `argument-hint`。模板支持带引号参数、`$1`、
`$2`、`$@`、`$ARGUMENTS`、`${1:-src}` 形式的默认值和 `${@:2}` 形式的切片。
`/commands` 会显示当前确定性目录和优先级作用域。内置命令不能被覆盖，文件与目录数量有硬
上限；自动加载的项目配置若指定外部 `commands.paths`，必须先显式信任。

Prompt 命令只在交互 Chat 中展开；Benchmark 的提示词、工具和缓存命名空间保持不变。

### 生命周期 Hooks

WeCode 为 `SessionStart`、`UserPromptSubmit`、`Stop` 和 `SessionEnd` 提供有边界的命令
Hooks。每个 Hook 会从 stdin 收到一个小型 JSON 对象，其中只有事件名、Session ID、
Workspace、服务商、模型、来源以及该事件需要的 Prompt 或停止原因。子进程不会继承已知模型
服务商的 API Key 环境变量。

Hooks 支持正则匹配器、Windows 专用命令、独立超时、输出硬上限、可见状态消息、fail-open /
fail-closed 和非阻塞异步通知。异步 Hook 不允许 fail-closed，因为其结果不会被等待。

退出码 `2`、`{"continue":false}` 或 `{"decision":"block"}` 可以阻止 Prompt，或要求 Agent
在停止前继续工作。成功的 JSON 输出可以通过 `additionalContext` 加入有大小限制的模型上下文；
`suppressOutput` 可隐藏常规 stdout。Stop Hook 最多连续触发三次，避免死循环。`/hooks` 会显示
当前事件目录。

自动加载的项目配置不能直接执行 Hooks。请先审查项目 `.wecode.toml`，再通过 `--config`
显式传入；或把可信 Hooks 放在 `~/.wecode/config.toml`。

Hooks 只在交互模式启用。`wecode run` 和 `wecode bench` 可能会反序列化共用配置，但不会发现、
构造或执行 Hooks；项目中的 Hook 声明也不会改变 benchmark 的工具注册表或 Prompt 路径。

## 权限审批

Shell 命令会分为 `read-only`、`workspace-write` 和 `elevated`。交互审批会显示完整命令或
Patch 摘要，并支持：

- `/approve`：仅允许本次执行。
- `/approve-session`：在 WeCode 退出前记住相同命令或工作区 Patch 授权。
- `/deny [原因]`：拒绝操作，并把原因反馈给模型，让它选择更安全的方案。
- `Ctrl-C`：取消当前运行，同时保留尚未投递的队列消息。

可选策略：

- `on-request`：默认；网络访问、进程控制、发布、系统包安装等 elevated 操作需要审批。
- `untrusted`：仅自动允许已知只读操作；工作区写入和 Patch 都需要审批。
- `never`：绝不弹出审批；除非同时使用 `--unsafe-local`，危险命令拦截仍然生效。

可以用 `--approval-policy` 覆盖配置。非交互 JSONL 运行绝不会等待审批人：需要审批的操作
会直接以拒绝结果反馈给模型。`wecode bench` 在没有显式策略时默认使用 `never`，因为
Benchmark Worker 应由外层容器或虚拟机负责隔离。

## 接入 OpenAI 兼容网关

大多数第三方网关应选择 Chat Completions：

```bash
export WECODE_API_KEY="your-gateway-key"

wecode run -C /path/to/repository \
  --provider openai \
  --base-url https://gateway.example/v1 \
  --model provider-model-id \
  --wire-api chat-completions \
  "完成需求并运行测试。"
```

默认启用原生 Function Tools。如果网关拒绝工具 Schema，或只返回纯文本工具调用，
可增加 `--text-actions`。交互模式默认启用模型流式响应；如果网关只支持完整响应，
可增加 `--no-stream`。

## 缓存机制

本地缓存键包含模型服务配置和完整请求，因此只会复用完全相同的模型响应。仓库发生变化后，
命令输出和文件读取结果不会跨状态缓存。

缓存模式：

- `read-write`：读取已有缓存并保存新响应。
- `read-only`：只重放已有缓存，不写入新条目。
- `refresh`：忽略已有缓存并覆盖结果。
- `off`：关闭精确响应缓存，适合冷启动测量。

```bash
wecode cache stats
wecode cache prune --max-megabytes 1024
wecode run --cache-mode off -C /path/to/repository "执行一次冷启动评测。"
```

## Benchmark 用法

Manifest 是 JSONL 文件，每条记录指向一个已准备好的独立工作区：

```json
{"id":"task-001","task":"修复失败的解析器测试。","workspace":"/work/repo","verify":"cargo test","max_steps":40}
```

顺序执行任务：

```bash
wecode bench tasks.jsonl \
  --provider openrouter \
  --model anthropic/claude-sonnet-4.6 \
  --output results.jsonl \
  --unsafe-local
```

接入 SWE-bench 等外部 Harness 时，每个实例应在隔离环境中运行，并收集结构化结果和最终补丁：

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

JSONL 和 Benchmark 输出会有意使用完整模型响应，并且不输出 UI 流式增量事件，即使配置中
`streaming = true`；Benchmark 运行也不会创建交互输入队列，因此 Prompt、轨迹和执行结果
保持稳定且便于机器解析。

WeCode 不会重置或清理工作树。仓库检出、任务隔离和评分应由 Benchmark Harness 负责。
更完整的建议见 [docs/benchmarking.md](docs/benchmarking.md)。

## 命令一览

```text
wecode run        执行一个自主编码任务
wecode chat       启动交互会话
wecode resume     恢复最近或指定会话
wecode sessions   列出当前工作区的已保存会话
wecode bench      执行 JSONL 任务清单
wecode providers  查看模型服务预设
wecode cache      查看或清理响应缓存
wecode init       创建初始配置
```

使用 `wecode <command> --help` 查看全部选项。

## 安全说明

Shell 权限能力很强，WeCode 本身不提供操作系统级沙箱。请在一次性容器或虚拟机中运行
不可信任务和带 `--unsafe-local` 的任务。常见模型 API Key 变量不会传递给子进程。

## 开源协议

WeCode 使用 [MIT License](LICENSE)。随项目分发的第三方组件所需声明位于
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
