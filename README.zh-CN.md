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
工作区、会话和协议；中间的可滚动时间线会区分用户消息、模型状态、Shell 命令、文件编辑、
命令输出、验证和最终回答；底部带边框的输入框在 Agent 工作时仍可继续编辑。

```text
/new         创建新的持久化会话
/resume [id] 恢复最近或指定会话
/sessions    列出当前工作区的最近会话
/rename      设置当前会话标题
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
/rules       查看已加载的项目规则
/config      查看当前配置文件
/history     查看输入历史文件
/help        查看所有命令
/quit        退出
```

会话自动保存在 `~/.wecode/sessions/chat/`。也可以在终端使用 `wecode resume [会话ID]`
恢复会话，或用 `wecode sessions` 查看最近会话。输入历史保存在 `~/.wecode/history`。
任务运行中按一次 `Ctrl-C` 会取消当前模型请求或 Shell 命令并返回输入框；已经应用的修改
会保留。Agent 工作时输入框仍然可编辑：普通 `Enter` 会 steer 当前任务，`Ctrl-J` 插入换行，
`Alt-Enter` 会把消息放入 follow-up 队列，`PageUp`/`PageDown` 可滚动时间线；不支持组合键的
终端可使用 `/steer` 和 `/followup`。Steer 只在安全的模型边界注入，不会在 Patch 应用到一半
时中断。不支持全屏界面的终端可设置 `WECODE_TUI=0` 使用普通行模式。交互渲染和模型流式事件与
`wecode run --output jsonl`、`wecode bench`
完全分离，因此 Benchmark 的事件输出和 Agent 执行不依赖终端 UI 或流式增量事件。

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
