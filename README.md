# model-toolcall-adapter-rs

Turn upstream models without native tool calling into standard tool-capable endpoints for Codex, OpenAI SDKs, and Anthropic-shaped clients.

`v0.2.0` · Rust 2021 · local-first · Responses / Chat Completions / Messages · DeepSeek Web

[简体中文](README.zh-CN.md) · [Download v0.2.0](https://github.com/openaeon/model-toolcall-adapter-rs/releases/tag/v0.2.0) · [Architecture](docs/ARCHITECTURE.zh-CN.md)

## What It Does

Many models can reason, code, and decide when a tool is needed, but do not reliably speak OpenAI-standard `tools`, `function_call`, or `tool_calls`.

This adapter does one thing:

```text
standard tools request
  -> render a text tool protocol
  -> send it to a plain upstream model
  -> parse the model's tool intent
  -> return standard function_call / tool_calls / Responses output
```

It does not execute user tools and does not replace Codex or your agent runtime. Codex, your desktop client, your backend, or your own runtime still performs real tool execution.

## Which File To Download

On the Release page, do not download GitHub's auto-generated `Source code` archives. They are source snapshots, not runnable binaries.

| Platform | Download | Use case |
| --- | --- | --- |
| Windows x64 | `model-toolcall-adapter-rs-v0.2.0-windows-x64-exe.zip` | Windows desktop or server |
| macOS Apple Silicon | `model-toolcall-adapter-rs-v0.2.0-macos-arm64.tar.gz` | M1 / M2 / M3 / M4 Macs |
| Linux x64 | `model-toolcall-adapter-rs-v0.2.0-linux-x64-server.tar.gz` | Common x86_64 Linux servers |
| Linux ARM64 | `model-toolcall-adapter-rs-v0.2.0-linux-arm64-server.tar.gz` | ARM64 Linux servers |
| Checksums | `SHA256SUMS.txt` | Verify downloaded packages |

Release URL:

```text
https://github.com/openaeon/model-toolcall-adapter-rs/releases/tag/v0.2.0
```

## Run In Three Steps

### 1. Start The Service

Windows PowerShell:

```powershell
Expand-Archive .\model-toolcall-adapter-rs-v0.2.0-windows-x64-exe.zip
cd .\model-toolcall-adapter-rs-windows-x64
.\model-toolcall-adapter-rs.exe
```

Windows CMD:

```bat
cd model-toolcall-adapter-rs-windows-x64
model-toolcall-adapter-rs.exe
```

macOS:

```bash
tar -xzf model-toolcall-adapter-rs-v0.2.0-macos-arm64.tar.gz
cd model-toolcall-adapter-rs-macos-arm64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

Linux x64:

```bash
tar -xzf model-toolcall-adapter-rs-v0.2.0-linux-x64-server.tar.gz
cd model-toolcall-adapter-rs-linux-x64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

Linux ARM64:

```bash
tar -xzf model-toolcall-adapter-rs-v0.2.0-linux-arm64-server.tar.gz
cd model-toolcall-adapter-rs-linux-arm64
chmod +x ./model-toolcall-adapter-rs
./model-toolcall-adapter-rs
```

If port `8787` is already in use:

```bash
ADAPTER_BIND=127.0.0.1:8899 ./model-toolcall-adapter-rs
```

### 2. Open The Setup Wizard

Open:

```text
http://127.0.0.1:8787/ui
```

First run creates:

```text
~/.model-toolcall-adapter/config.json
```

It includes a generated `adapter_api_key` for local API authentication.

### 3. Choose An Upstream

The wizard walks through:

1. Select `openai-compatible` or `deepseek-web`.
2. For DeepSeek Web, start the controlled browser and log in.
3. Capture the session, then copy the Base URL, adapter key, model name, or apply Codex config.

## UI Preview

![Setup wizard screenshot](docs/assets/setup-wizard.png)

## Capability Map

| Area | Supported | Notes |
| --- | --- | --- |
| Client APIs | Responses / Chat Completions / Messages | Codex, OpenAI-compatible, and Anthropic-shaped clients |
| Tool calls | XML / JSON / tolerant text parsing | Emits standard tool calls; does not execute them |
| Responses state | retrieve / delete / input_items / input_tokens / cancel / compact | Supports `previous_response_id` and Conversations |
| Streaming | Responses SSE | Chat / Messages streaming is not yet incremental |
| Long tasks | `background: true` | Retrieve polling and cancellation |
| Structured output | `json_object` / common `json_schema` | Not a complete JSON Schema engine |
| Reasoning | Reasoning/text separation | `reasoning.encrypted_content` is a local opaque placeholder or pass-through |
| DeepSeek Web | Login, session, PoW, SSE, uploads | Search, reasoning, expert, vision, and file-mode mapping |
| Codex | One-click config | Backs up and writes `~/.codex/config.toml` and `auth.json` |
| Local state | JSON + lock + atomic replacement | Reduces corruption risk after crashes or multiple local processes |

## Good Fit / Bad Fit

| Good fit | Bad fit |
| --- | --- |
| Strong upstream models that do not reliably support function calling | Running shell, browser, database, or business tools inside the adapter |
| DeepSeek Web, Ollama, vLLM, or LM Studio behind Codex-style clients | Replacing OpenAI-hosted `file_search` or vector stores |
| Clients that already send OpenAI `tools` to a plain-text upstream | Exact OpenAI server-side Structured Outputs compatibility |
| One bridge for Responses, Chat Completions, and Messages | Shared distributed response state across multiple remote nodes |

## Codex

The setup wizard can write Codex configuration for you. It backs up:

```text
~/.codex/config.toml
~/.codex/auth.json
```

Core config:

```toml
model_provider = "ModelToolCallAdapter"

[model_providers.ModelToolCallAdapter]
name = "ModelToolCallAdapter"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
requires_openai_auth = true
```

`auth.json`:

```json
{
  "OPENAI_API_KEY": "adp_xxx"
}
```

Restart Codex after applying the config.

## Providers

### OpenAI-Compatible

Works with Ollama, vLLM, LM Studio, llama.cpp server, and OpenAI Chat Completions-compatible upstreams.

```bash
ADAPTER_UPSTREAM_BASE_URL=http://127.0.0.1:11434/v1 \
ADAPTER_UPSTREAM_MODEL=qwen3-coder \
cargo run
```

Model aliases:

```bash
ADAPTER_MODEL_ALIASES=gpt-5-codex=qwen3-coder,gpt-5-mini=qwen3-fast cargo run
```

When the client requests `gpt-5-codex`, the adapter forwards to `qwen3-coder` and maps the response model name back.

### DeepSeek Web

DeepSeek Web is an unofficial web upstream. The adapter reads only its own controlled browser profile, not the user's normal browser cookies.

Models:

| Model | Meaning |
| --- | --- |
| `deepseek-web/reasoner` | reasoning |
| `deepseek-web/chat` | normal chat |
| `deepseek-web/search` | search-enabled mode |
| `deepseek-web/expert` | expert mode |
| `deepseek-web/vision` | vision and file path |

Session file:

```text
~/.model-toolcall-adapter/deepseek_session.json
```

If DeepSeek Web changes its private API, headers, PoW, or SSE shape, this provider may need updates.

## API Examples

### Responses Tool Call

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer adp_xxx' \
  -d '{
    "model": "deepseek-web/reasoner",
    "input": "Use a tool when external information is required.",
    "tools": [{
      "type": "function",
      "name": "search_web",
      "description": "Search by query",
      "parameters": {
        "type": "object",
        "properties": {
          "query": { "type": "string" }
        },
        "required": ["query"]
      }
    }]
  }'
```

The adapter may return:

```json
{
  "object": "response",
  "status": "completed",
  "output": [{
    "type": "function_call",
    "status": "completed",
    "call_id": "call_1",
    "name": "search_web",
    "arguments": "{\"query\":\"...\"}"
  }]
}
```

Continue after executing the tool:

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer adp_xxx' \
  -d '{
    "model": "deepseek-web/reasoner",
    "previous_response_id": "resp_xxx",
    "input": [{
      "type": "function_call_output",
      "call_id": "call_1",
      "output": "Tool result"
    }]
  }'
```

### Chat Completions Tool Call

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer adp_xxx' \
  -d '{
    "model": "deepseek-web/chat",
    "messages": [{ "role": "user", "content": "Check Beijing weather" }],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_weather",
        "parameters": {
          "type": "object",
          "properties": { "city": { "type": "string" } },
          "required": ["city"]
        }
      }
    }]
  }'
```

### Responses Background Tasks

Start a background response:

```json
{
  "model": "deepseek-web/reasoner",
  "input": "Run a long analysis",
  "background": true
}
```

Poll:

```bash
curl http://127.0.0.1:8787/v1/responses/resp_xxx \
  -H 'authorization: Bearer adp_xxx'
```

Cancel:

```bash
curl -X POST http://127.0.0.1:8787/v1/responses/resp_xxx/cancel \
  -H 'authorization: Bearer adp_xxx'
```

## Images, Files, And Local File Search

Use standard Responses content parts:

```json
{
  "model": "deepseek-web/vision",
  "input": [{
    "type": "message",
    "role": "user",
    "content": [
      { "type": "input_text", "text": "Inspect this image" },
      { "type": "input_image", "image_url": "data:image/png;base64,..." }
    ]
  }]
}
```

DeepSeek Web uploads attachments internally, waits for parsing/readiness, and sends private ids upstream. Expert mode does not directly support file references, so file-bearing requests are bridged through the vision or file path when needed.

Responses `tools:[{"type":"file_search"}]` searches only readable `input_file.file_data` text from the current request. It does not read arbitrary local files and is not a durable vector store.

## Configuration

| Setting | Environment variable | Default |
| --- | --- | --- |
| Bind address | `ADAPTER_BIND` | `127.0.0.1:8787` |
| OpenAI-compatible upstream | `ADAPTER_UPSTREAM_BASE_URL` | `http://127.0.0.1:11434/v1` |
| Upstream API key | `ADAPTER_UPSTREAM_API_KEY` | empty |
| Upstream model | `ADAPTER_UPSTREAM_MODEL` | `local-model` |
| Model aliases | `ADAPTER_MODEL_ALIASES` | empty |
| Adapter API key | `ADAPTER_API_KEY` | local config |
| Max tools | `ADAPTER_MAX_TOOL_DEFINITIONS` | `64` |
| Request timeout | `ADAPTER_REQUEST_TIMEOUT_SECS` | `120` |
| Config file | `ADAPTER_CONFIG_FILE` | `~/.model-toolcall-adapter/config.json` |
| Response store | `ADAPTER_RESPONSE_STORE_FILE` | `~/.model-toolcall-adapter/responses_store.json` |
| Conversation store | `ADAPTER_CONVERSATION_STORE_FILE` | `~/.model-toolcall-adapter/conversations_store.json` |
| DeepSeek session | `ADAPTER_DEEPSEEK_SESSION_FILE` | `~/.model-toolcall-adapter/deepseek_session.json` |

Per-request overrides:

```http
x-upstream-provider: deepseek-web
x-upstream-base-url: https://api.example.com/v1
x-upstream-api-key: sk-...
x-deepseek-session: {"cookie":"..."}
```

## Endpoints

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Health check |
| `GET /ui` | Setup wizard |
| `GET /v1/models` | Model list |
| `POST /v1/chat/completions` | Chat Completions |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | Responses create |
| `GET /v1/responses/{id}` | Retrieve response |
| `DELETE /v1/responses/{id}` | Delete response |
| `GET /v1/responses/{id}/input_items` | Response input items |
| `POST /v1/responses/{id}/cancel` | Cancel background response |
| `POST /v1/responses/input_tokens` | Estimate input tokens |
| `POST /v1/responses/compact` | Compact response context |
| `POST /v1/conversations` | Create conversation |
| `GET /v1/conversations/{id}` | Retrieve conversation |
| `POST /v1/conversations/{id}` | Update metadata |
| `DELETE /v1/conversations/{id}` | Delete conversation |
| `GET /v1/conversations/{id}/items` | List items |
| `POST /v1/conversations/{id}/items` | Append items |
| `GET /v1/conversations/{id}/items/{item_id}` | Retrieve item |
| `DELETE /v1/conversations/{id}/items/{item_id}` | Delete item |
| `GET /setup/state` | Setup state |
| `POST /setup/provider` | Save provider |
| `POST /setup/deepseek-browser/start` | Start DeepSeek login browser |
| `POST /setup/deepseek-browser/capture` | Capture DeepSeek session |
| `POST /setup/codex/apply` | Write Codex config |

## Run From Source

```bash
git clone https://github.com/openaeon/model-toolcall-adapter-rs.git
cd model-toolcall-adapter-rs
cargo run
```

Verify:

```bash
cargo fmt -- --check
cargo test
cargo build
```

## Packaging

Prepare targets:

```bash
rustup target add aarch64-apple-darwin
rustup target add x86_64-pc-windows-gnu
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild
brew install zig
```

Build:

```bash
cargo build --release --target aarch64-apple-darwin
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

Package outputs live in:

```text
dist/packages/
```

Commit only compressed archives and `SHA256SUMS.txt`, not `dist/work/` or unpacked temporary directories.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| `Address already in use` | Stop the old process or use `ADAPTER_BIND=127.0.0.1:8899 ./model-toolcall-adapter-rs` |
| Windows says the command is not recognized | Enter the exe directory and run `.\model-toolcall-adapter-rs.exe` or `model-toolcall-adapter-rs.exe` |
| DeepSeek login browser prints GCM/DEPRECATED_ENDPOINT logs | Usually Chrome background service logs, not a login failure |
| `/v1/models` does not return DeepSeek models | Capture and save the DeepSeek session in `/ui` first |
| Codex does not use the adapter | Restart Codex and check `~/.codex/config.toml` and `auth.json` |

## Boundaries

- The adapter does not execute user business tools.
- The adapter reads only its own controlled browser profile for DeepSeek login capture.
- `reasoning.encrypted_content` is a local opaque placeholder or pass-through, not OpenAI server-side encryption.
- `json_schema` support covers common Structured Outputs constraints, not the complete JSON Schema specification.
- DeepSeek Web depends on private web APIs and may require maintenance when the website changes.

## License

MIT
