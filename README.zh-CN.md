# model-toolcall-adapter-rs

[English](README.md) | [简体中文](README.zh-CN.md)

> 一个独立 Rust 适配器，让只会输出文本的模型也能接入 Codex 风格、OpenAI-compatible、Anthropic 风格的编程客户端。

`model-toolcall-adapter-rs` 对外提供 OpenAI-compatible 与 Anthropic 风格 HTTP 端点，把标准工具定义转换成稳定的文本协议，发给上游普通文本模型，再把模型输出的工具意图解析回标准工具调用响应。

它面向那些代码推理能力不错、但不稳定支持原生 function calling / tool calling 的模型和服务。

项目目标是对齐主流编程 agent 与编辑器工具：期望 OpenAI Responses 的 Codex 风格客户端、使用 Anthropic/Claude Messages 形态的客户端，以及可以配置 OpenAI-compatible `base_url` 的开发工具。

## 功能概览

- 提供 `POST /v1/chat/completions`，兼容 OpenAI Chat Completions 客户端。
- 提供 `POST /v1/responses`，并支持 retrieve、input-items、cancel、compact 等 Responses 端点。
- 提供 `POST /v1/messages`，兼容 Anthropic Messages 风格请求。
- 将 OpenAI function tools 转成模型可读的 XML/text 工具协议。
- 从纯文本模型输出中容错解析 XML、JSON 和常见 tool-call 形态。
- 支持 OpenAI-compatible 上游，例如本地 Ollama、vLLM、LM Studio、llama.cpp 风格 API。
- 内置 DeepSeek Web 上游 provider，支持本地 session 存储、PoW、SSE 解析、reasoning/text 分离。
- 内置 `/ui` 启动向导，用于选择供应商、生成 adapter key、登录 DeepSeek Web 并展示桥接接口。

## 截图演示

![启动向导演示](docs/assets/setup-wizard.png)

本项目是独立仓库。构建和运行时不依赖 `../crates/aeon-claw-api`、`aeon-claw-cli` 或 FCACoreai workspace。

## 兼容目标

这个 adapter 是协议桥，不要求工具或编辑器专门适配本项目；只要客户端能说下面任一 HTTP 格式，就可以接入。

| 客户端类型 | 预期接口 | Adapter 端点 |
| --- | --- | --- |
| Codex 风格编程 agent | OpenAI Responses 风格 API | `/v1/responses` |
| OpenAI-compatible 编程工具 | Chat Completions API | `/v1/chat/completions` |
| Anthropic/Claude 风格客户端 | Messages 形态请求 | `/v1/messages` |
| 支持自定义 base URL 的编辑器/终端 agent | OpenAI-compatible `base_url` | `http://127.0.0.1:8787/v1` |
| 本地模型服务 | Ollama、vLLM、LM Studio、llama.cpp 风格 OpenAI API | 上游 `ADAPTER_UPSTREAM_BASE_URL` |

它适合作为 Codex-compatible CLI、Claude/Anthropic 风格 agent runtime、Cursor/Continue 类编辑器集成、Aider/OpenCode 类终端 agent，以及企业内部 agent 平台的本地协议桥。实际兼容性取决于客户端是否允许配置自定义 base URL，以及它使用的 wire format。

本项目不是 OpenAI、Anthropic、Cursor、Continue、Aider、Cline 或 OpenCode 的官方集成。它是一个本地协议适配层，用来帮助这些类型的工具连接只能返回纯文本的上游模型。

## 架构

```text
编程客户端 / Agent Runtime
        |
        | Codex-style Responses
        | OpenAI Chat Completions
        | Anthropic-style Messages
        v
model-toolcall-adapter-rs
        |
        | tool schema -> text tool protocol
        | model text -> standard tool calls
        v
上游模型
        |
        | OpenAI-compatible API
        | DeepSeek Web
        v
纯文本模型输出
```

核心模块保持小而清晰：

- `src/wire/*`：处理请求/响应 wire format 转换。
- `src/protocol/mod.rs`：渲染文本工具协议，并解析模型输出里的工具调用。
- `src/upstream.rs`：路由到 OpenAI-compatible 或 DeepSeek Web 上游。
- `src/providers/deepseek_web/`：独立 DeepSeek Web provider、session、PoW 与 SSE 解析。
- `src/responses_store.rs`：内存态 Responses 存储，用于 retrieve、input-items、cancel 和多轮续接。

## 快速开始

### 使用发行包

运行下方“打包教程”后，预构建包会生成在 `dist/packages`：

```text
dist/packages/
├── model-toolcall-adapter-rs-windows-x64-exe.zip
├── model-toolcall-adapter-rs-macos-arm64.tar.gz
├── model-toolcall-adapter-rs-linux-x64-server.tar.gz
├── model-toolcall-adapter-rs-linux-arm64-server.tar.gz
└── SHA256SUMS.txt
```

Windows：

```powershell
Expand-Archive .\model-toolcall-adapter-rs-windows-x64-exe.zip
cd .\model-toolcall-adapter-rs-windows-x64-exe\model-toolcall-adapter-rs-windows-x64
.\model-toolcall-adapter-rs.exe
```

Windows 终端不会自动从当前目录查找程序，所以必须带 `.\` 前缀。DeepSeek Web 常见 `sha256` PoW 已由 adapter 内置 Rust 实现处理，不需要用户额外安装 Node.js。

macOS Apple Silicon：

```bash
tar -xzf model-toolcall-adapter-rs-macos-arm64.tar.gz
cd model-toolcall-adapter-rs-macos-arm64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

Linux 服务器 x64：

```bash
tar -xzf model-toolcall-adapter-rs-linux-x64-server.tar.gz
cd model-toolcall-adapter-rs-linux-x64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

Linux 服务器 ARM64：

```bash
tar -xzf model-toolcall-adapter-rs-linux-arm64-server.tar.gz
cd model-toolcall-adapter-rs-linux-arm64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

然后打开：

```text
http://127.0.0.1:8787/ui
```

首次启动按页面向导操作：选择供应商、登录 DeepSeek Web、捕获 session、查看 Adapter Key，并可一键写入 Codex 配置。

### 从源码运行

```bash
git clone https://github.com/openaeon/model-toolcall-adapter-rs.git
cd model-toolcall-adapter-rs
cargo run
```

打开内置 UI：

```text
http://127.0.0.1:8787/ui
```

如果 `8787` 端口被占用：

```bash
ADAPTER_BIND=127.0.0.1:8899 cargo run
```

然后打开：

```text
http://127.0.0.1:8899/ui
```

首次启动会自动创建本地配置：

```text
~/.model-toolcall-adapter/config.json
```

其中会随机生成 `adapter_api_key`。打开 `/ui` 后按向导完成：

- 第一步选择供应商：`openai-compatible` 或 `deepseek-web`。
- 第二步 DeepSeek Web 会启动独立浏览器 profile 登录，并从这个受控浏览器捕获 session。
- 第三步展示 Base URL、Adapter Key、模型名和请求示例，也可以一键写入 Codex 配置。

如果要直接指定 OpenAI-compatible 上游，也可以用：

```bash
cargo run -- \
  --bind 127.0.0.1:8787 \
  --upstream-base-url http://127.0.0.1:11434/v1 \
  --upstream-model qwen3-coder \
  --model-aliases codex-adapter=qwen3-coder
```

## Codex 一键配置

启动向导第三步的“一键配置 Codex”会：

- 备份 `~/.codex/config.toml` 和 `~/.codex/auth.json`。
- 在 `config.toml` 顶部写入 adapter 的模型选择，并在文件末尾写入 provider 表。
- 将当前随机 `adapter_api_key` 写入 `auth.json` 的 `OPENAI_API_KEY`。

写入的 provider 使用 Codex 官方支持的 Responses wire：

```toml
[model_providers.ModelToolCallAdapter]
name = "ModelToolCallAdapter"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
requires_openai_auth = true
```

如果 Codex CLI/app 已经在运行，配置后需要重启。

如果 Codex 桌面端提示“无法更新模型设置”，先打开 `/ui` 第三步点击“一键配置 Codex”，再检查 `~/.codex/config.toml` 顶部是否已经写入 `model_provider = "ModelToolCallAdapter"`，以及 `~/.codex/auth.json` 里的 `OPENAI_API_KEY` 是否为 `adp_` 开头的 adapter key。受控 Chrome 偶尔输出 `Registration URL fetching failed`、`DEPRECATED_ENDPOINT`、`ConnectionHandler failed with net error` 这类 GCM/后台联网日志，通常只是 Chrome 自身服务噪声，不代表 DeepSeek session 捕获失败。

## 打包教程

本项目是单个 Rust 二进制。发行包只包含二进制文件和一个简单的 `README.txt`。

准备工具：

```bash
rustup target add aarch64-apple-darwin
rustup target add x86_64-pc-windows-gnu
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild
brew install zig
```

编译二进制：

```bash
cargo build --release --target aarch64-apple-darwin
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

生成发行包：

```bash
mkdir -p dist/packages \
  dist/work/model-toolcall-adapter-rs-macos-arm64 \
  dist/work/model-toolcall-adapter-rs-linux-x64 \
  dist/work/model-toolcall-adapter-rs-linux-arm64 \
  dist/work/model-toolcall-adapter-rs-windows-x64

cp target/aarch64-apple-darwin/release/model-toolcall-adapter-rs \
  dist/work/model-toolcall-adapter-rs-macos-arm64/model-toolcall-adapter-rs
cp target/x86_64-unknown-linux-musl/release/model-toolcall-adapter-rs \
  dist/work/model-toolcall-adapter-rs-linux-x64/model-toolcall-adapter-rs
cp target/aarch64-unknown-linux-musl/release/model-toolcall-adapter-rs \
  dist/work/model-toolcall-adapter-rs-linux-arm64/model-toolcall-adapter-rs
cp target/x86_64-pc-windows-gnu/release/model-toolcall-adapter-rs.exe \
  dist/work/model-toolcall-adapter-rs-windows-x64/model-toolcall-adapter-rs.exe

for d in dist/work/model-toolcall-adapter-rs-*; do
  cat > "$d/README.txt" <<'EOF'
Model Toolcall Adapter RS

Run:
  macOS/Linux:
    chmod +x ./model-toolcall-adapter-rs
    ./model-toolcall-adapter-rs

  Windows:
    .\model-toolcall-adapter-rs.exe

Default UI:
  http://127.0.0.1:8787/ui

Local config:
  ~/.model-toolcall-adapter/config.json
EOF
done

(cd dist/work && tar -czf ../packages/model-toolcall-adapter-rs-macos-arm64.tar.gz model-toolcall-adapter-rs-macos-arm64)
(cd dist/work && tar -czf ../packages/model-toolcall-adapter-rs-linux-x64-server.tar.gz model-toolcall-adapter-rs-linux-x64)
(cd dist/work && tar -czf ../packages/model-toolcall-adapter-rs-linux-arm64-server.tar.gz model-toolcall-adapter-rs-linux-arm64)
(cd dist/work && zip -qr ../packages/model-toolcall-adapter-rs-windows-x64-exe.zip model-toolcall-adapter-rs-windows-x64)
shasum -a 256 dist/packages/* > dist/packages/SHA256SUMS.txt
```

打包需要几 GB 可用磁盘空间，因为 Cargo 会为每个 target 保存构建产物。如果遇到 `No space left on device`，保留 `dist/packages`，删除 `target/` 下失败 target 的目录，再一次只编译一个 target。

## 配置

每个 CLI 参数都有对应环境变量。CLI/env 显式值优先于本地配置文件。

```bash
export ADAPTER_BIND=127.0.0.1:8787
export ADAPTER_UPSTREAM_BASE_URL=http://127.0.0.1:11434/v1
export ADAPTER_UPSTREAM_API_KEY=
export ADAPTER_UPSTREAM_MODEL=qwen3-coder
export ADAPTER_MODEL_ALIASES=codex-adapter=qwen3-coder
export ADAPTER_API_KEY=local-dev-key
export ADAPTER_DEEPSEEK_SESSION_FILE=~/.model-toolcall-adapter/deepseek_session.json
cargo run
```

如果没有设置 `ADAPTER_API_KEY`，adapter 会读取或创建：

```text
~/.model-toolcall-adapter/config.json
```

并使用其中的随机 `adapter_api_key` 保护 API。

`ADAPTER_MODEL_ALIASES` 是逗号分隔的映射：

```text
外部模型名=上游真实模型名,另一个外部模型名=另一个上游真实模型名
```

例如：

```bash
export ADAPTER_MODEL_ALIASES=gpt-5-codex=deepseek-web/reasoner,gpt-5-mini=deepseek-web/chat
```

客户端可以请求 `model: "gpt-5-codex"`，adapter 会转发到 `deepseek-web/reasoner`，并在响应里恢复外部模型名。

## 鉴权

如果 `ADAPTER_API_KEY` 和本地配置里的 `adapter_api_key` 都为空，adapter 端点不鉴权。

如果设置了 `ADAPTER_API_KEY`，请求需要携带以下任一 header：

```http
Authorization: Bearer local-dev-key
```

或：

```http
x-api-key: local-dev-key
```

每次请求都可以覆盖上游配置：

```http
x-upstream-base-url: https://api.example.com/v1
x-upstream-api-key: sk-...
x-upstream-provider: openai-compatible
```

DeepSeek Web 请求：

```http
x-upstream-provider: deepseek-web
x-deepseek-session: {"cookie":"...","bearer":"...","last_session_id":"..."}
```

如果省略 `x-deepseek-session`，adapter 会读取：

```text
~/.model-toolcall-adapter/deepseek_session.json
```

也可以设置 `ADAPTER_DEEPSEEK_SESSION_FILE` 指向其他路径。

## DeepSeek Web

DeepSeek Web 支持完全在当前仓库内实现。

UI 会通过 `/setup/deepseek-browser/start` 启动一个独立浏览器 profile，并开启 DevTools 调试端口。用户在这个受控浏览器里登录 DeepSeek Web 后，点击捕获 session，adapter 会把 DeepSeek cookie/localStorage 中可用凭据保存到：

```text
~/.model-toolcall-adapter/deepseek_session.json
```

如果本机没有 Chrome/Edge/Chromium/Brave，或调试端口不可用，UI 会保留手动粘贴 Session JSON/Cookie 的 fallback。

保存对象可以包含：

```json
{
  "cookie": "ds_session=...; ...",
  "bearer": "optional-token",
  "user_agent": "Mozilla/5.0 ...",
  "base_url": "https://chat.deepseek.com",
  "last_session_id": "optional-session-id"
}
```

DeepSeek Web 是非官方网页上游。如果网页服务调整私有端点、headers 或 PoW 行为，这个 provider 可能需要更新。

## 端点

| 端点 | 用途 |
| --- | --- |
| `GET /health` | 健康检查 |
| `GET /ui` | 内置调试 UI |
| `GET /v1/models` | 列出上游模型和别名模型 |
| `POST /v1/chat/completions` | OpenAI Chat Completions-compatible 请求 |
| `POST /v1/messages` | Anthropic Messages 风格请求 |
| `POST /v1/responses` | OpenAI Responses 风格创建 response |
| `GET /v1/responses/{response_id}` | 读取内存中的 response |
| `GET /v1/responses/{response_id}/input_items` | 列出保存的 response 输入项 |
| `POST /v1/responses/{response_id}/cancel` | 取消 background response |
| `POST /v1/responses/compact` | 压缩 response 上下文 |
| `GET /setup/state` | 读取启动向导状态和本地配置 |
| `POST /setup/provider` | 保存供应商选择 |
| `POST /setup/deepseek-browser/start` | 启动受控浏览器登录 DeepSeek Web |
| `POST /setup/deepseek-browser/capture` | 从受控浏览器捕获并保存 DeepSeek session |
| `POST /setup/codex/apply` | 备份并写入 Codex config/auth |
| `POST /deepseek-web/login` | 打开 DeepSeek 登录页 |
| `POST /deepseek-web/session` | 本地保存 DeepSeek session |

为了兼容 base URL 直接指向 host 的客户端，Responses 相关路由也提供了不带 `/v1` 的版本。

## Chat Completions 示例

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-dev-key' \
  -d '{
    "model": "qwen3-coder",
    "messages": [
      { "role": "user", "content": "查一下北京天气" }
    ],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get weather by city",
        "parameters": {
          "type": "object",
          "properties": {
            "city": { "type": "string" }
          },
          "required": ["city"]
        }
      }
    }]
  }'
```

如果上游模型输出：

```xml
<tool_call id="call_1" name="get_weather">{"city":"北京"}</tool_call>
```

adapter 会在 Chat Completions 响应中返回标准 `tool_calls`。

## Responses 工具闭环

第一轮请求：

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-dev-key' \
  -d '{
    "model": "qwen3-coder",
    "input": "查一下北京天气",
    "tools": [{
      "type": "function",
      "name": "get_weather",
      "description": "Get weather by city",
      "parameters": {
        "type": "object",
        "properties": {
          "city": { "type": "string" }
        },
        "required": ["city"]
      }
    }]
  }'
```

如果模型输出工具调用，adapter 会返回：

```json
{
  "object": "response",
  "status": "completed",
  "output": [{
    "type": "function_call",
    "status": "completed",
    "call_id": "call_1",
    "name": "get_weather",
    "arguments": "{\"city\":\"北京\"}"
  }]
}
```

客户端执行工具后，继续请求：

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-dev-key' \
  -d '{
    "model": "qwen3-coder",
    "previous_response_id": "resp_xxx",
    "input": [{
      "type": "function_call_output",
      "call_id": "call_1",
      "output": "北京今天晴，气温 12-20 摄氏度"
    }]
  }'
```

adapter 会把上一轮输入、上一轮模型输出和新的工具结果拼入下一次上游 prompt。

## 工具执行边界

adapter 默认不执行业务工具。

它的职责是把用户传入的工具 schema 转成模型可理解的提示，把普通文本模型输出解析回标准工具调用，并接收调用方回传的 `function_call_output` 继续多轮对话。真实工具应由你的 agent runtime、应用服务或客户端执行。

## 开发

```bash
cargo fmt --check
cargo check
cargo test
```

本项目刻意保持为单个独立 Rust binary crate。

## 当前边界

已实现：

- Chat Completions、Messages、Responses 兼容；Responses `stream: true` 会立即建立 SSE、发送 in-progress 保活、reasoning summary 事件和最终文本/工具调用事件。
- Responses create、retrieve、input-items、cancel、compact 端点。
- `previous_response_id` 多轮续接，并在 adapter 进程内复用 DeepSeek Web chat session。
- Responses 顶层 `function_call` 输出与 `function_call_output` 续接。
- `tool_choice` 基础语义：`auto`、`none`、`required`、指定 function name，以及 `parallel_tool_calls` 裁剪。
- 模型别名。
- Adapter API key 鉴权。
- 按请求覆盖上游 base URL 和 API key。
- DeepSeek Web 受控浏览器登录、Cookie/localStorage 捕获、session 保存/读取、PoW、completion、文本解析。
- Codex 一键配置：备份并写入 `~/.codex/config.toml` 和 `auth.json`，使用 Responses wire。
- Codex 请求摘要日志：模型、stream、previous response、工具名、tool_choice、输入大小和脱敏上游选项。
- XML 与容错 JSON 工具调用解析。

暂未实现：

- Chat Completions / Messages 的真实增量流式输出。
- Responses 的真实上游 token-by-token 转发；当前是在上游请求期间保持 SSE 连接并发送保活，完成后输出最终 reasoning/text/tool-call 事件。
- 进程内存之外的持久 response 存储。

## License

MIT
