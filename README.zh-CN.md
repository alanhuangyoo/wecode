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
- **工具调用可靠**：原生提供 `shell`、`apply_patch`、`finish`，不支持工具调用的网关可用
  JSON 文本动作协议兜底。
- **专门设计的终端交互**：支持对话历史、多轮上下文、工具与输出卡片、Thinking 状态和
  斜杠命令，同时避免笨重的全屏 TUI。
- **持久化会话**：对话自动保存为可读的 JSONL，可列出和恢复；多轮对话使用稳定的服务端
  缓存标识，避免每轮破坏 Prompt Cache。
- **理解仓库规范**：从仓库根目录到当前工作区分层加载受限大小的 `AGENTS.md`、
  `CLAUDE.md` 和 rules 目录。
- **高缓存命中设计**：稳定的 Prompt 前缀、服务端 Prompt Cache 提示，以及本地精确响应缓存，
  让重复运行更快、更省 Token。
- **适合跑榜**：支持非交互运行、JSONL 事件与轨迹、最终 Git Patch、验证重试和 JSONL
  批量任务。
- **默认行为可预测**：限制最大步骤、执行时间和命令输出，并隔离 API Key、拦截明显危险命令。

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

交互模式会保留多轮上下文，下一轮也能看到前面已完成的修改。模型思考、Shell 命令、
文件编辑、命令输出、验证和最终回答会显示在不同的终端卡片中。

```text
/new         创建新的持久化会话
/resume [id] 恢复最近或指定会话
/sessions    列出当前工作区的最近会话
/rename      设置当前会话标题
/status      查看模型、工作区、缓存和上下文
/rules       查看已加载的项目规则
/config      查看当前配置文件
/history     查看输入历史文件
/help        查看所有命令
/quit        退出
```

会话自动保存在 `~/.wecode/sessions/chat/`。也可以在终端使用 `wecode resume [会话ID]`
恢复会话，或用 `wecode sessions` 查看最近会话。输入历史保存在 `~/.wecode/history`。
交互渲染与 `wecode run --output jsonl`、`wecode bench`
完全分离，因此 Benchmark 的事件输出和 Agent 执行不依赖终端 UI。

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
可增加 `--text-actions`。

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
