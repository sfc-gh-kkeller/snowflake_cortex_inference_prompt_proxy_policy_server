## Snowflake Cortex AI Agent Compatibility Proxy

A high-performance Rust proxy that lets any AI coding agent (Claude Code, OpenCode, ZeroClaw, Continue.dev, Mistral Vibe, etc.) use Snowflake Cortex AI models.

### What it does

1. **API Translation** — Accepts requests in Anthropic or OpenAI format and translates them to Snowflake Cortex's API, so any AI tool can use Snowflake as its LLM backend without modification.

2. **Agent API Proxying** — Proxies Snowflake's Cortex Agent API (`/agent:run`), handling the PAT-to-session-token exchange automatically. Also provides translation endpoints that accept Anthropic or OpenAI format and route through the Agent API.

3. **Policy Enforcement (Optional)** — When enabled, intercepts prompts on **all routes** before they reach the LLM and evaluates them against configurable security policies. Uses a two-tier approach: fast pattern matching first, then a Cortex judge model for deeper evaluation. Blocks prompt injection, data exfiltration attempts, unauthorized tool use, and other threats. **Omit the `[policy]` section entirely to run as a pure API proxy with no policy overhead.**

### Why use this

- Use any coding agent while centralizing inference in Snowflake Cortex
- Keep AI and data governance in Snowflake Horizon catalog
- Combine with Snowflake MCP server for native data access
- Centralized billing across all Snowflake-supported models

![Architecture](architecture_ai_layer.png)

---

### Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  AI Client (Claude Code, OpenCode, ZeroClaw, Continue.dev, etc.)    │
└─────────┬───────────┬───────────┬──────────┬──────────┬─────────────┘
          │           │           │          │          │
  Anthropic API   OpenAI API   OpenAI API  Anthropic  agent:run
  /v1/messages    /v1/chat/    /v1/chat/   /v1/       /agent:run
                  completions  completions/ messages/  (native)
                               agent       agent
          │           │           │          │          │
┌─────────▼───────────▼───────────▼──────────▼──────────▼─────────────┐
│                         CORTEX PROXY (port 8766)                     │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │              Policy Enforcement Engine (optional)               │  │
│  │  Two-tier: pattern matching (fast) → judge model (thorough)    │  │
│  │  Evaluates prompts on ALL routes against configurable rules    │  │
│  │  Actions: BLOCK (HTTP 403) | WARN (log + allow) | LOG          │  │
│  └────────────────────────────────────────────────────────────────┘  │
│                                                                      │
│  ┌──────────────────────┐  ┌──────────────────────────────────────┐  │
│  │  Format Translation  │  │  Authentication Layer                │  │
│  │                      │  │                                      │  │
│  │  Anthropic → OpenAI  │  │  PAT → Bearer token (chat/compl.)   │  │
│  │  OpenAI → OpenAI     │  │  PAT → Session token (agent:run)    │  │
│  │  Anthropic → agent   │  │  Session passthrough (if provided)  │  │
│  │  OpenAI → agent      │  │  Token caching with expiry          │  │
│  └──────────────────────┘  └──────────────────────────────────────┘  │
└─────────────────────────────────┬────────────────────────────────────┘
                                  │
                    ┌─────────────▼─────────────┐
                    │  Snowflake Cortex APIs     │
                    │                           │
                    │  /api/v2/cortex/v1/       │
                    │    chat/completions       │
                    │                           │
                    │  /api/v2/cortex/          │
                    │    agent:run              │
                    └───────────────────────────┘
```

---

### Route Map

| Route | Method | Input Format | Backend | Auth Mode | Policy |
|---|---|---|---|---|---|
| `/health` | GET | — | Local | None | No |
| `/v1/messages` | POST | Anthropic Messages API | `chat/completions` | PAT (auto) | Yes* |
| `/v1/chat/completions` | POST | OpenAI Chat API | `chat/completions` | PAT (auto) | Yes* |
| `/agent:run` | POST | Native Cortex agent:run | `agent:run` | Session (auto-exchange or passthrough) | Yes* |
| `/v1/messages/agent` | POST | Anthropic Messages API | `agent:run` (translated) | Session (auto-exchange) | Yes* |
| `/v1/chat/completions/agent` | POST | OpenAI Chat API | `agent:run` (translated) | Session (auto-exchange) | Yes* |
| `/*` (wildcard) | ANY | OpenAI Chat API | `chat/completions` | PAT (auto) | Yes* |

> **\*Policy column**: Policy enforcement applies to all routes when the `[policy]` section is present and `enabled = true`. Omit the `[policy]` section entirely to disable — no performance overhead when disabled.

> **`force_agent_backend = true`**: When this flag is set in `[agent]`, the `/v1/messages` and `/v1/chat/completions` routes automatically delegate to the agent:run translation handlers. All requests get policy enforcement and agent:run features with zero client-side changes.

---

### Quick start

#### Install (scripts)

#### macOS / Linux

```bash
curl -sSLO https://raw.githubusercontent.com/sfc-gh-kkeller/snowflake_cortex_inference_prompt_proxy_policy_server/main/install.sh
chmod +x install.sh
./install.sh
```

#### Windows (PowerShell)

```powershell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/sfc-gh-kkeller/snowflake_cortex_inference_prompt_proxy_policy_server/main/install.bat" -OutFile "install.bat"
.\install.bat
```

After install, edit the config at `~/.config/cortex-proxy/config.toml` and set your Snowflake `base_url`, `pat`, and `default_model`.

### Manual install

Download the binary for your platform from [GitHub Releases](https://github.com/sfc-gh-kkeller/snowflake_cortex_inference_prompt_proxy_policy_server/releases):

| Platform | Download |
|----------|----------|
| **macOS ARM64** | `cortex-proxy-macos-arm64` |
| **macOS Intel** | `cortex-proxy-macos-x64` |
| **Linux x64** | `cortex-proxy-linux-x64` |
| **Linux ARM64** | `cortex-proxy-linux-arm64` |
| **Windows x64** | `cortex-proxy-windows-x64.exe` |
| **Windows ARM64** | `cortex-proxy-windows-arm64.exe` |

**Bundles with example config:**
- `cortex-proxy-linux-x64-bundle.tar.gz`
- `cortex-proxy-linux-arm64-bundle.tar.gz`

Steps:
1. Download and extract the binary
2. Move to a directory on your PATH (`~/.local/bin` or `/usr/local/bin`)
3. Create config: `~/.config/cortex-proxy/config.toml`
4. Copy from `cortex-proxy.example.toml` and set your credentials
5. Run: `cortex-proxy`

---

### Configuration

Create a config file (**do not commit your PAT**). Default location: `~/.config/cortex-proxy/config.toml`

Sample `cortex-proxy.toml`:

```toml
[proxy]
port = 8766
log_level = "info"
timeout_secs = 300
connection_pool_size = 10

[snowflake]
base_url = "https://<account>.snowflakecomputing.com/api/v2/cortex/v1"
pat = "<YOUR_PAT>"
default_model = "claude-opus-4-5"

# Required for agent:run (PAT→session token exchange)
login_name = "YOUR_SNOWFLAKE_USERNAME"
account_name = "YOUR_ACCOUNT_LOCATOR"

[model_map]
# Optional: client model name -> Snowflake model name
# "claude-opus-4-5" = "claude-opus-4-5"
# "gpt-4" = "claude-4-sonnet"
```

**Finding your values:**
- `base_url`: Replace `<account>` with your Snowflake account locator (e.g., `ORGNAME-ACCOUNTNAME`)
- `pat`: The PAT secret string from Snowflake UI → Admin → Security → Programmatic Access Tokens
- `login_name`: Your Snowflake login username (often an email address)
- `account_name`: Same as the account locator in the URL (e.g., `ORGNAME-ACCOUNTNAME`)

#### Policy Enforcement (Optional)

Policy enforcement is **entirely optional**. Without the `[policy]` section, the proxy runs as a pure API translator with zero policy overhead.

To enable, add to your config:

```toml
[policy]
enabled = true
judge_model = "claude-4-sonnet"
action = "block"                   # "block" | "warn" | "log"
policies_file = "policies.toml"    # or define rules inline
```

Make sure `policies.toml` exists in the same directory (it ships with the project).

When enabled, every prompt on every route goes through a **two-tier evaluation**:
1. **Pattern matching** (fast, case-insensitive) — catches obvious violations like "ignore all previous instructions" with near-zero latency
2. **Judge model** (thorough) — only invoked if pattern matching doesn't catch the prompt; sends the prompt to a Cortex model for deeper analysis

Optionally combine with the agent backend for full agent:run features:

```toml
[agent]
enabled = true
force_agent_backend = true   # route ALL traffic through agent:run
```

Run the proxy:

```bash
cortex-proxy
# Or with explicit config:
cortex-proxy --config /path/to/config.toml
```

### Config search order

1. `--config <path>` CLI flag (recommended)
2. `CORTEX_PROXY_CONFIG` environment variable
3. `~/Library/Application Support/cortex-proxy/config.toml` (macOS)
4. `~/.config/cortex-proxy/config.toml` (Linux)
5. `./cortex-proxy.toml` (current directory)

Policy rules (`policies.toml`) are searched in:

1. `./policies.toml` (current directory)
2. `~/.config/cortex-proxy/policies.toml`

### Environment variables

The proxy itself only reads `CORTEX_PROXY_CONFIG`. Most clients still require an API key env var, but the proxy ignores it.

```bash
# Proxy config (optional if using default search order)
export CORTEX_PROXY_CONFIG=/path/to/config.toml

# Anthropic-compatible clients (Claude Code, etc.)
export ANTHROPIC_BASE_URL=http://localhost:8766
export ANTHROPIC_API_KEY=dummy-key-proxy-handles-auth

# OpenAI-compatible clients
export OPENAI_BASE_URL=http://localhost:8766
export OPENAI_API_KEY=dummy-key-proxy-handles-auth
```

---

### Authentication Flow

#### Chat/Completions (PAT Auth)

For `/v1/messages`, `/v1/chat/completions`, and wildcard routes, the proxy authenticates to Snowflake using a **Programmatic Access Token (PAT)** as a Bearer token:

```
Client → Proxy → Snowflake
         Authorization: Bearer <PAT>
         X-Snowflake-Authorization-Token-Type: PROGRAMMATIC_ACCESS_TOKEN
```

The client does not need any Snowflake credentials. The client's `Authorization` header (e.g., `x-api-key: dummy`) is ignored — the proxy substitutes its own PAT.

#### Agent:run (Session Token Auth)

The Cortex Agent API (`/agent:run`) requires a **Snowflake session token**, not a PAT. The proxy handles this automatically:

```
1. Client sends request to proxy (no Snowflake auth needed)
2. Proxy checks session token cache
3. If expired or missing:
   POST /session/v1/login-request
   Body: { ACCOUNT_NAME, LOGIN_NAME, AUTHENTICATOR: "SNOWFLAKE_JWT", TOKEN: <PAT> }
   → Receives session token (valid ~3600s)
4. Proxy caches token, forwards request with:
   Authorization: Snowflake Token="<session_token>"
```

Alternatively, if the client already has a session token, it can pass it directly:
```
Authorization: Snowflake Token="<session_token>"
```
The proxy detects this and skips the exchange.

---

### How the Translation Works

#### Anthropic → Snowflake Cortex (`/v1/messages`)

The proxy receives an Anthropic Messages API request and converts it to OpenAI `chat/completions` format:

- `messages[].content` (string or content blocks) → OpenAI `messages[].content`
- `system` → `messages[0].role = "system"`
- `tool_use` / `tool_result` content blocks → OpenAI `tool_calls` / `tool` role messages
- Response SSE: Snowflake returns OpenAI-format chunks, proxy passes them through

#### Anthropic → Agent API (`/v1/messages/agent`)

The proxy receives an Anthropic Messages API request, converts it to Cortex `agent:run` format, and maps the response SSE back:

**Request translation:**
- `system` prompts → prepended to first user message with `<system>` tags
- `tool_use` content blocks → `{"type": "tool_use", "id": ..., "name": ..., "input": ...}`
- `tool_result` content blocks → `{"type": "tool_result", "tool_use_id": ..., "content": ...}`
- `tools` array → wrapped in `{"tool_spec": {...}}` for agent:run format

**Response SSE mapping:**
| Cortex agent:run Event | Anthropic Event |
|---|---|
| `message.delta` (type=text) | `content_block_start` + `content_block_delta` (text_delta) |
| `message.delta` (type=tool_use) | `content_block_start` (tool_use) + `content_block_delta` (input_json_delta) |
| `done` | `content_block_stop` + `message_delta` + `message_stop` |

#### OpenAI → Agent API (`/v1/chat/completions/agent`)

Same pattern as above but with OpenAI format on both ends:

**Response SSE mapping:**
| Cortex agent:run Event | OpenAI Event |
|---|---|
| `message.delta` (type=text) | `data: {"choices":[{"delta":{"content":"..."}}]}` |
| `message.delta` (type=tool_use) | `data: {"choices":[{"delta":{"tool_calls":[...]}}]}` |
| `done` | `data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + `data: [DONE]` |

---

### Model Mapping

The proxy maps client-requested model names to Snowflake Cortex model names. Default mappings are built in:

| Client Sends | Snowflake Gets |
|---|---|
| `claude-4-sonnet`, `claude-3-5-sonnet` | `claude-4-sonnet` |
| `claude-opus-4-5`, `claude-4-5-opus` | `claude-opus-4-5` |
| `claude-4-opus` | `claude-4-opus` |
| `claude-haiku-4-5`, `haiku` | `claude-haiku-4-5` |
| Any unrecognized model | `claude-4-sonnet` (default) |

Custom mappings can be added in the config:

```toml
[model_map]
"gpt-4" = "claude-4-sonnet"
"gpt-4o" = "claude-opus-4-5"
```

---

### Policy Enforcement Engine (Optional)

When enabled, the policy engine intercepts prompts on **all routes** (`/v1/messages`, `/v1/chat/completions`, `/agent:run`, `/v1/messages/agent`, `/v1/chat/completions/agent`, and wildcard) before forwarding to Snowflake. **Omit the `[policy]` section to disable entirely — no performance cost when disabled.**

#### How It Works

1. Extract the user's prompt text from the request body
2. **Tier 1 — Pattern matching**: Check the prompt against known violation patterns from rule examples (case-insensitive). If matched, act immediately with no LLM call
3. **Tier 2 — Judge model**: If pattern matching passes, build a policy evaluation prompt containing all active rules, their descriptions, and example violations. Send to Snowflake Cortex `chat/completions` API with the configured judge model
4. Parse the judge model's response: `ALLOW` or `DENY:<rule_name>:<reason>`
5. If `DENY` and action is `block`: return HTTP 403 to the client
6. If `DENY` and action is `warn`: log the violation, forward the request
7. If `DENY` and action is `log`: silently log, forward the request

#### Policy Rules (policies.toml)

Six rules are included by default:

| Rule | Severity | What It Detects |
|---|---|---|
| `prompt_injection` | critical | Attempts to override system instructions or manipulate agent behavior |
| `data_exfiltration` | critical | Attempts to extract credentials, secrets, PII, or bulk data |
| `unauthorized_tool_use` | high | Attempts to invoke destructive or out-of-scope tools |
| `code_execution_abuse` | high | Attempts to execute malicious code, establish persistence, or escalate privileges |
| `pii_exposure` | medium | Requests that could expose PII without authorization |
| `scope_violation` | medium | Attempts to make the agent operate outside its designated function |

Each rule has:
- `enabled`: toggle on/off
- `severity`: critical / high / medium
- `description`: what the judge model evaluates against
- `examples`: concrete violation patterns

#### Adding Custom Rules

Add a new section to `policies.toml`:

```toml
[policy.rules.financial_advice]
enabled = true
severity = "high"
description = "Detect attempts to get the agent to provide financial advice or investment recommendations"
examples = [
    "Should I buy this stock?",
    "What investments would you recommend?",
    "Is now a good time to sell my portfolio?",
]
```

Restart the proxy to pick up changes.

---

### Policy Server (Optional)

For production deployments where policies need to be managed centrally, a FastAPI policy server is included:

```bash
cd policy-server
pip install fastapi uvicorn
python server.py
# Runs on http://localhost:8900
```

**Endpoints:**

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Health check |
| GET | `/policies` | List all policy rules |
| GET | `/policies/{name}` | Get a specific rule |
| PUT | `/policies/{name}` | Update a rule |
| POST | `/evaluate` | Evaluate a prompt against rules |
| POST | `/reload` | Reload policies from disk |

The policy server provides a REST API for managing rules without restarting the proxy. To use it, set `source = "remote"` and `server_url` in the proxy config.

---

### Connecting AI Tools

#### Claude Code

Set the API base URL to the proxy:

```bash
export ANTHROPIC_BASE_URL=http://localhost:8766
export ANTHROPIC_API_KEY=dummy-key-proxy-handles-auth
```

Claude Code sends requests to `/v1/messages` — the proxy translates and forwards to Snowflake Cortex.

![Claude Code via Cortex Proxy](claude_code_cortex_proxy.png)

#### Mistral Vibe

Mistral Vibe reads `config.toml` from `./.vibe/config.toml` or `~/.vibe/config.toml`.

Add a proxy provider + model, then set the active model (use a model your Snowflake account is allowed to access):

```toml
[[providers]]
name = "cortex-proxy"
api_base = "http://localhost:8766"
api_key_env_var = "CORTEX_PROXY_API_KEY"
api_style = "openai"
backend = "generic"

[[models]]
name = "claude-opus-4-5"
provider = "cortex-proxy"
alias = "claude-opus-4-5-cortex"
temperature = 0.2
input_price = 0.0
output_price = 0.0

active_model = "claude-opus-4-5-cortex"
```

Then set a dummy API key (the proxy ignores it):

```bash
export CORTEX_PROXY_API_KEY=dummy-key-proxy-handles-auth
```

If you see `403 Forbidden` with `Model <name> not allowed`, switch to a model your Snowflake account has access to (e.g., `claude-4-sonnet`).

![Mistral Vibe via Cortex Proxy](mistral_vibe_code_cortex_proxy.png)

#### OpenCode

Add a provider entry pointing to the proxy in your global config:

```jsonc
{
  "provider": {
    "cortex-proxy": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Cortex Proxy (Localhost)",
      "options": {
        "baseURL": "http://localhost:8766",
        "apiKey": "local"
      },
      "models": {
        "claude-4-opus": { "name": "Claude 4 Opus (via proxy)", "tool_call": true, "attachment": true },
        "claude-4-sonnet": { "name": "Claude 4 Sonnet (via proxy)", "tool_call": true, "attachment": true }
      }
    }
  },
  "model": "cortex-proxy/claude-4-opus"
}
```

Then run:

```bash
opencode
```

![OpenCode via Cortex Proxy](Opencode_cortex_proxy.png)

#### ZeroClaw

ZeroClaw is a lightweight Rust-based coding agent. Configure it in `~/.zeroclaw/config.toml`:

```toml
default_provider = "custom:http://localhost:8766/v1"
default_model = "claude-4-sonnet"
api_key = "dummy-key"

[agent]
max_tool_iterations = 10

[autonomy]
level = "supervised"
allowed_commands = ["ls", "cat", "echo", "pwd", "git", "grep", "find", "wc"]
```

ZeroClaw sends OpenAI-format requests to `/v1/chat/completions` — the proxy translates and forwards to Snowflake Cortex.

#### Continue.dev

```yaml
name: Snowflake Cortex Config
version: 1.0.0
schema: v1

models:
  - name: Claude Opus 4.5 (Cortex)
    provider: openai
    model: claude-opus-4-5
    apiBase: http://localhost:8766
    apiKey: dummy-key-proxy-handles-auth
    useLegacyCompletionsEndpoint: false
    roles:
      - chat
      - edit
      - apply
    capabilities:
      - tool_use

  - name: Claude 4 Sonnet (Cortex)
    provider: openai
    model: claude-4-sonnet
    apiBase: http://localhost:8766
    apiKey: dummy-key-proxy-handles-auth
    useLegacyCompletionsEndpoint: false
    roles:
      - chat
      - edit
      - apply
    capabilities:
      - tool_use

  - name: Claude 3.5 Sonnet (Cortex)
    provider: openai
    model: claude-3-5-sonnet
    apiBase: http://localhost:8766
    apiKey: dummy-key-proxy-handles-auth
    useLegacyCompletionsEndpoint: false
    roles:
      - chat
      - edit
      - apply
    capabilities:
      - tool_use

  - name: Claude Haiku 4.5 (Cortex)
    provider: openai
    model: claude-haiku-4-5
    apiBase: http://localhost:8766
    apiKey: dummy-key-proxy-handles-auth
    useLegacyCompletionsEndpoint: false
    roles:
      - chat
      - edit
      - apply
      - autocomplete
    capabilities:
      - tool_use

tabAutocompleteModel:
  provider: openai
  model: claude-haiku-4-5
  apiBase: http://localhost:8766
  apiKey: dummy-key-proxy-handles-auth
  useLegacyCompletionsEndpoint: false
```

![Continue.dev via Cortex Proxy](continue_dev_cortex_proxy.png)

#### Force Agent Backend (Recommended for Coding Agents)

Coding agents like Claude Code, Continue.dev, and Cursor don't know about the `/agent` endpoints — they only hit standard `/v1/messages` or `/v1/chat/completions`. To transparently route all traffic through agent:run (with policy enforcement), set:

```toml
[agent]
enabled = true
force_agent_backend = true
```

With this flag:
- `/v1/messages` → delegates to the Anthropic→agent:run translation handler
- `/v1/chat/completions` → delegates to the OpenAI→agent:run translation handler
- Every request gets policy evaluation before reaching Snowflake
- The response is translated back to the original API format
- **No client-side changes required** — the coding agent doesn't know it's going through agent:run

#### Direct agent:run Access

For custom applications that speak the Cortex Agent API natively:

```bash
curl -X POST http://localhost:8766/agent:run \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
    "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
    "model": "auto",
    "stream": true,
    "origin_application": "my_app",
    "tools": [],
    "tool_choice": {"type": "auto"}
  }'
```

---

### Testing

#### Test Anthropic API

```bash
curl -sS http://localhost:8766/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: dummy" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"claude-opus-4-5","max_tokens":128,"messages":[{"role":"user","content":"Say hi from the Cortex proxy."}]}'
```

#### Test OpenAI API

```bash
curl -sS http://localhost:8766/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer dummy" \
  -d '{"model":"claude-opus-4-5","messages":[{"role":"user","content":"Say hi from the Cortex proxy."}],"max_tokens":128}'
```

#### Test suite

```bash
pixi run python test_proxy.py
```

Expected output:

```
🧪 Testing Cortex Proxy at http://localhost:8766

  ✓ Health check
  ✓ Anthropic /v1/messages
  ✓ OpenAI /v1/chat/completions
  ✓ Agent:run /agent:run (native)
  ✓ Anthropic→Agent /v1/messages/agent
  ✓ OpenAI→Agent /v1/chat/completions/agent
  ✓ Policy enforcement blocked malicious prompt (HTTP 403)

Results: 7 passed, 0 failed, 7 total
```

Use `--skip-agent` to skip agent:run tests (useful if `login_name`/`account_name` aren't configured):

```bash
pixi run python test_proxy.py --skip-agent
```

---

### Build from source

```bash
pixi run cargo build --release --manifest-path cortex-proxy-rs/Cargo.toml
# Binary at: cortex-proxy-rs/target/release/cortex-proxy
```

### Project structure

```
cortex-proxy-rs/           # Rust source
├── src/main.rs            # All proxy logic (~2100 lines)
├── Cargo.toml
├── target/release/        # Compiled binary
cortex-proxy.example.toml  # Template config
policies.toml              # Security policy rules (6 rules)
policy-server/
└── server.py              # FastAPI policy management server
test_proxy.py              # Python test client (7 tests)
pixi.toml                  # Pixi environment config
install.sh                 # macOS/Linux installer
install.bat                # Windows installer
```

---

### Security Considerations

- **PAT in config**: The `cortex-proxy.toml` file contains your PAT. Treat it like a password. Use `--config` to point to a secured location, or use environment variables.
- **Policy eval adds latency**: Each agent:run request with policy enabled makes an additional LLM call (~1-3s). For latency-sensitive workloads, consider `action = "log"` instead of `"block"`.
- **Judge model choice**: The judge model (`judge_model` in config) should be fast and accurate. `claude-4-sonnet` is a good default. Using a smaller model reduces latency but may increase false positives/negatives.
- **Session tokens**: Cached in memory with expiry tracking. If the proxy restarts, a new session token is obtained on the next agent:run request.
- **No TLS termination**: The proxy listens on plain HTTP. For production, put it behind a reverse proxy (nginx, Caddy) with TLS, or use it only on localhost.
- **CORS**: The proxy allows all origins by default for development. Restrict this in production.

---

### Troubleshooting

**Proxy loads wrong config file**
Use `--config /path/to/cortex-proxy.toml` explicitly. The proxy prints which config it loaded at startup.

**Policy rules show 0 active**
Check that `policies.toml` is in the current working directory or `~/.config/cortex-proxy/`. The proxy only loads rules from the file if the `[policy]` section in the main config has no inline `[policy.rules.*]`.

**Agent:run returns 401**
Verify `login_name` and `account_name` are set in the config. These are required for PAT→session token exchange.

**Agent:run returns 400**
Check that your PAT is valid and not expired. The proxy logs the error body — look for specific Snowflake error messages.

**Empty responses from translation endpoints**
The proxy expects `event: message.delta` SSE events from Cortex agent:run. If Snowflake changes the SSE format, the translation handlers need updating.

**Policy eval always allows**
Check proxy logs for `Policy eval HTTP` errors. Common issues: wrong `judge_model` name, PAT expired, or Snowflake API changes.

**Build fails**
Make sure pixi is installed: `curl -fsSL https://pixi.sh/install.sh | bash`. Then `pixi run cargo build --release --manifest-path cortex-proxy-rs/Cargo.toml`.

---

### Links

- [Claude Code](https://www.anthropic.com/claude-code)
- [OpenCode](https://opencode.ai)
- [Continue.dev](https://continue.dev)
- [Mistral Vibe](https://mistral.ai)
- [ZeroClaw](https://github.com/sfc-gh-kkeller/zeroclaw)
- [Snowflake Cortex AI](https://docs.snowflake.com/en/user-guide/snowflake-cortex)
