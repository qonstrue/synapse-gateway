# synapse-gateway

synapse-gateway is an OpenAI-compatible LLM router and gateway written in Rust. It accepts standard OpenAI `POST /v1/chat/completions` requests and routes them through config-driven fallback chains to one of two backend lanes: a standard lane (via the `genai` crate, supporting OpenAI, Qwen/DashScope, and other OpenAI-compatible providers) or a native Vertex AI lane (using raw HTTP to the Vertex REST API with support for cached content, Cloud Storage media URIs, and strict response schemas). Prometheus metrics and OpenTelemetry `gen_ai.*` span attributes are emitted for every request, and a per-tenant cost ledger records token usage events to SQLite or Postgres.

---

## Architecture: two lanes

### Standard lane

Requests without a `vertex` extension block are handled by the standard lane, which uses the [`genai`](https://crates.io/crates/genai) crate as its HTTP adapter. Any provider reachable via an OpenAI-compatible API (OpenAI, Qwen/DashScope, self-hosted vLLM/Ollama/TGI via `oai_compat`) can appear in a fallback chain.

### Native Vertex lane

If the request body contains a `vertex` extension object with any of `cached_content`, `media_uris`, or `response_schema`, the request is routed to the native Vertex lane. This lane speaks directly to the Vertex AI `generateContent` REST endpoint, translating the OpenAI message format while preserving Vertex-specific features:

- **`cached_content`** — a `cachedContents` resource name for context caching.
- **`media_uris`** — `gs://` Cloud Storage URIs attached as inline parts.
- **`response_schema`** — a JSON schema passed as `generationConfig.responseSchema` for constrained decoding.

A route leg that is reachable only by the standard lane (i.e. has no `vertex` leg configured) will return `400 Bad Request` if a native-Vertex request is sent against it.

### Lane detection

```json
{
  "model": "gemini-pro",
  "messages": [...],
  "vertex": {
    "cached_content": "projects/my-project/locations/us-central1/cachedContents/abc123",
    "media_uris": ["gs://my-bucket/file.mp4"],
    "response_schema": { "type": "object", "properties": { "answer": { "type": "string" } } }
  }
}
```

The presence of the `vertex` key (any of its fields) is the sole signal. Requests without it always go to the standard lane.

---

## Quick start

### Prerequisites

Set the credentials for every provider referenced in your `config/routes.toml`:

```bash
# Vertex AI (Application Default Credentials are used via google-cloud-auth)
export VERTEX_PROJECT=my-gcp-project

# Qwen / DashScope
export DASHSCOPE_API_KEY=sk-...
# export DASHSCOPE_BASE_URL=https://dashscope.aliyuncs.com/compatible-mode/v1  # optional

# OpenAI
export OPENAI_API_KEY=sk-...
# export OPENAI_BASE_URL=https://api.openai.com/v1  # optional

# OAI-compatible self-hosted (vLLM / Ollama / TGI)
export OAI_COMPAT_BASE_URL=http://localhost:8000/v1
# export OAI_COMPAT_API_KEY=token-xyz  # optional
```

### Run

```bash
cargo run --release
# Server: 0.0.0.0:8080
# Prometheus: 0.0.0.0:9090
```

### Standard request

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Streaming request

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Count to 5."}],
    "stream": true
  }'
```

Responses are Server-Sent Events (SSE) in the standard OpenAI `data: {...}` format, terminated by `data: [DONE]`.

### Native Vertex request

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{
    "model": "gemini-pro",
    "messages": [{"role": "user", "content": "Describe this video."}],
    "vertex": {
      "media_uris": ["gs://my-bucket/video.mp4"]
    }
  }'
```

---

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Returns `200 OK` with `{"status":"ok"}`. |
| `GET` | `/v1/models` | Lists all model aliases defined in `routes.toml`. |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat completions. Supports `stream: true` (SSE). Accepts optional `vertex` extension block. |

---

## Configuration

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SYNAPSE_ADDR` | `0.0.0.0:8080` | Address and port for the main HTTP server. |
| `SYNAPSE_METRICS_ADDR` | `0.0.0.0:9090` | Address and port for the Prometheus metrics endpoint. |
| `SYNAPSE_ROUTES_PATH` | `config/routes.toml` | Path to the route configuration file. |
| `SYNAPSE_PRICING_PATH` | `config/pricing.toml` | Path to the pricing configuration file. |
| `SYNAPSE_LEDGER_BACKEND` | `sqlite` | Cost ledger backend: `sqlite` or `postgres`. |
| `SYNAPSE_LEDGER_DSN` | `sqlite://synapse.db?mode=rwc` | Database connection string for the ledger. |
| `SYNAPSE_DEFAULT_TENANT` | `unattributed` | Tenant name used when `x-synapse-tenant` header is absent. |
| `SYNAPSE_REQUEST_TIMEOUT_SECS` | `120` | Per-request timeout in seconds. |

### Provider credential variables

The gateway performs a fail-fast credential check at startup. If a provider is referenced in `routes.toml` but its required credentials are missing, the process exits immediately.

| Provider | Required | Optional |
|----------|----------|----------|
| `vertex` | `VERTEX_PROJECT` (ADC via `google-cloud-auth`) | — |
| `qwen` | `DASHSCOPE_API_KEY` | `DASHSCOPE_BASE_URL` |
| `openai` | `OPENAI_API_KEY` | `OPENAI_BASE_URL` |
| `oai_compat` | `OAI_COMPAT_BASE_URL` | `OAI_COMPAT_API_KEY` |

### `config/routes.toml`

Maps a client-facing model alias to an ordered list of fallback legs. The gateway tries each leg in order, advancing on error.

```toml
[routes."gemini-pro"]
legs = [
  { provider = "vertex", model = "gemini-3-pro" },
  { provider = "qwen",   model = "qwen-max" },
]

[routes."fast"]
legs = [{ provider = "vertex", model = "gemini-3-flash" }]
```

### `config/pricing.toml`

Maps `provider:model` to input/output cost in USD per 1,000,000 tokens. Models not listed cost 0.

```toml
# USD per 1,000,000 tokens. Open-source/self-hosted default to 0.
["vertex:gemini-3-pro"]
input  = 1.25
output = 5.0

["vertex:gemini-3-flash"]
input  = 0.30
output = 1.20

["qwen:qwen-max"]
input  = 1.6
output = 6.4
```

---

## Tenant attribution

Two request headers control cost and observability attribution:

| Header | Description |
|--------|-------------|
| `x-synapse-tenant` | Tenant identifier. Falls back to `SYNAPSE_DEFAULT_TENANT` (`unattributed`). |
| `x-synapse-workspace` | Optional sub-grouping within a tenant (e.g. a project or team). |

Both values are recorded on ledger `usage_events` rows and carried as attributes on `gen_ai.*` spans.

---

## Observability

### Prometheus

Metrics are served at `SYNAPSE_METRICS_ADDR` (default `:9090`).

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `synapse_requests_total` | Counter | `route`, `model`, `system`, `lane` | Total requests served. |
| `synapse_request_duration_seconds` | Histogram | `route`, `model`, `system`, `lane` | End-to-end request latency. |
| `synapse_input_tokens_total` | Counter | `route`, `model`, `system`, `lane` | Cumulative input tokens consumed. |
| `synapse_output_tokens_total` | Counter | `route`, `model`, `system`, `lane` | Cumulative output tokens generated. |
| `synapse_ledger_dropped_total` | Counter | — | Ledger events dropped due to a full channel (fire-and-forget overflow). |

All four `synapse_*` token/request metrics share the same label set:

- **`route`** — the client-facing model alias (e.g. `gemini-pro`, `fast`).
- **`model`** — the model that actually served the request (as returned by the backend leg).
- **`system`** — the OpenLLMetry `gen_ai.system` value: `vertexai`, `openai`, `dashscope`, or `oai_compat`.
- **`lane`** — `standard` (genai crate) or `native` (direct Vertex REST).

Tenant and workspace are **not** Prometheus labels. They are recorded in the cost ledger (`usage_events` table) and carried as attributes on `gen_ai.*` tracing spans. Keeping them out of metric labels avoids unbounded cardinality from untrusted client-supplied header values.

### Tracing

Structured spans follow the OpenTelemetry `gen_ai.*` semantic conventions (model, provider, token counts, error kinds). Configure the log level and format via `RUST_LOG` (e.g. `RUST_LOG=info`).

---

## Cost ledger

Token usage is recorded asynchronously to a `usage_events` table after every successful completion. The ledger write is fire-and-forget: if the internal channel is full, the event is dropped and `synapse_ledger_dropped_total` is incremented — request latency is never affected.

### Backends

| Backend | Cargo feature | Notes |
|---------|--------------|-------|
| SQLite | `ledger-sqlite` (default) | DSN default: `sqlite://synapse.db?mode=rwc`. File created automatically. |
| Postgres | `ledger-postgres` | Requires `SYNAPSE_LEDGER_DSN`. |

Only one backend feature may be active at a time. SQLite is enabled by default.

### Schema

The single migration (`migrations/0001_usage_events.sql`) creates the `usage_events` table with columns for tenant, workspace, provider, model, input tokens, output tokens, cost, and timestamp.

---

## Building

### Cargo

```bash
# Default build (SQLite ledger)
cargo build --release

# Postgres ledger (disables SQLite)
cargo build --release --no-default-features --features ledger-postgres
```

The release binary is at `target/release/synapse-gateway`.

### Docker

```bash
docker build -t synapse-gateway .
docker run --rm \
  -e VERTEX_PROJECT=my-project \
  -e OPENAI_API_KEY=sk-... \
  -p 8080:8080 \
  -p 9090:9090 \
  -v "$(pwd)/config:/app/config" \
  synapse-gateway
```

The multi-stage `Dockerfile` uses `rust:1-bookworm` to compile and `debian:bookworm-slim` as the runtime image. `config/` and `migrations/` are copied into the image so it is self-contained; mount a volume over `/app/config` to supply your own route/pricing files at runtime.

---

## Testing

```bash
# Run all tests (SQLite feature, default)
cargo test

# Run all tests with all features (SQLite + Postgres)
cargo test --all-features
```

The test suite (46 tests) covers route resolution, fallback behaviour, lane detection, tenant attribution, config parsing, ledger writes, and HTTP handler integration.

---

## Limitations / roadmap

The following are **not** present in v1 and are planned for future releases:

- Authentication / API key enforcement on inbound requests.
- Rate limiting.
- Multi-region Vertex endpoint routing.
- Admin API for dynamic route reloading.
