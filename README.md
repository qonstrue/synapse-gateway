# synapse-gateway

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![CI](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml)

**English** · [Español](README.es-ES.md)

synapse-gateway is an OpenAI-compatible LLM router and gateway written in Rust. It accepts standard OpenAI `POST /v1/chat/completions` requests and routes them through config-driven fallback chains to one of two backend lanes: a standard lane (via the `genai` crate, supporting OpenAI, Qwen/DashScope, and other OpenAI-compatible providers) or a native Vertex AI lane (using raw HTTP to the Vertex REST API with support for cached content, Cloud Storage media URIs, and strict response schemas). Prometheus metrics and OpenTelemetry `gen_ai.*` span attributes are emitted for every request, and a per-tenant cost ledger records token usage events to SQLite or Postgres.

---

## Why yet another LLM router/gateway?

The honest answer: we tried not to write one. We evaluated [`litellm-rs`](https://github.com/majiayu000/litellm-rs) (and the general "just put an OpenAI-compatible proxy in front of everything" approach) first — and it would have cost us the one thing we couldn't give up: **native Vertex AI**.

- **It doesn't flatten Vertex down to the lowest common denominator.** `litellm-rs` and most OpenAI-compatible gateways reach Vertex/Gemini only through a generic OpenAI-shaped adapter, which throws away the Vertex-specific features we actually depend on: context caching (`cachedContent`), Cloud Storage (`gs://`) media URIs, and strict native `responseSchema` constrained decoding. synapse keeps a dedicated **native Vertex lane** that speaks `:generateContent` / `:streamGenerateContent` directly, so those capabilities survive — while everything else still rides the standard OpenAI-compatible lane via [`genai`](https://crates.io/crates/genai). You get multi-provider routing *and* Vertex's native power, not one or the other.

- **It's small and owned, not a framework.** synapse is a single Rust binary — or an embeddable library crate (`default-features = false`, call `Gateway::chat()` in-process) — with a focused dependency set. Because we own the routing, fallback, ledger, and observability code, the things `other gateways` didn't offer were straightforward to add rather than upstream battles: a **per-tenant cost ledger** with multi-sink fan-out (SQLite/Postgres + Pub/Sub + SNS), and **OpenTelemetry `gen_ai.*` spans** + Prometheus metrics on every request.

- **The good bits are standard, not premium add-ons.** Streaming is real and on by default: the gateway always streams from upstream internally, so `stream: true` clients get token-by-token OpenAI-compatible SSE, and non-streaming clients get that same response buffered into one JSON object — which means they *keep full fallback across the whole chain*. **Tool / function calling works on both lanes.** And because the surface is plain OpenAI-compatible, existing OpenAI SDKs work unchanged. None of this is gated behind a tier; it's the baseline.

In short: synapse is the *simple* OpenAI-compatible gateway that doesn't make you trade away Vertex's native capabilities to get streaming, tool calling, multi-provider fallback, and cost accounting.

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

## Streaming & tool calling

### Streaming

Setting `"stream": true` returns an OpenAI-compatible Server-Sent Events response: a sequence of `chat.completion.chunk` events (each prefixed `data: `) terminated by `data: [DONE]`. Without it, a single `chat.completion` JSON object is returned.

Internally the gateway **always** streams from the upstream provider, even for non-streaming clients. Non-streaming responses are fully buffered before delivery, so the complete fallback chain (all legs) is available on any failure — including mid-stream failures on early legs.

### Tool calling

Tool calling is supported on both lanes:

- **Standard lane** — send OpenAI `tools` (array of `{type: "function", function: {name, description, parameters}}`) and optionally `tool_choice`. The gateway translates them for the `genai` crate. Note: `tool_choice` is best-effort on this lane; genai 0.6 `ChatRequest` has no `tool_choice` field, so it is not forwarded.
- **Native Vertex lane** — `tools` are translated to Vertex `functionDeclarations`; `tool_choice` is honored natively via `toolConfig.functionCallingConfig`.

Responses carry `tool_calls` on the assistant message and `finish_reason: "tool_calls"`. In streaming mode, tool-call deltas are emitted as indexed `chat.completion.chunk` events (same shape as the OpenAI streaming spec).

### Timeouts

Two environment variables bound stream latency:

| Variable | Default | Description |
|----------|---------|-------------|
| `SYNAPSE_REQUEST_TIMEOUT_SECS` | `120` | Maximum time to the first chunk (time-to-first-token). A leg that does not produce its first chunk within this window is abandoned and the chain falls back to the next leg. |
| `SYNAPSE_STREAM_IDLE_TIMEOUT_SECS` | `60` | Maximum gap between successive chunks. If no chunk arrives within this window after streaming has started, the leg is terminated with a mid-stream error. |

Both timeouts apply to the standard lane. The native Vertex lane is currently bounded only by the underlying HTTP client timeout (`SYNAPSE_REQUEST_TIMEOUT_SECS`); idle timeout and first-chunk fallback for that lane are a tracked follow-up.

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
| `SYNAPSE_LEDGER_BACKENDS` | `sqlite` | Comma-separated list of active ledger sinks (e.g. `postgres,pubsub`). Every event fans out to all listed sinks. |
| `SYNAPSE_LEDGER_BACKEND` | — | Single-backend alias; used when `SYNAPSE_LEDGER_BACKENDS` is not set. |
| `SYNAPSE_LEDGER_SQLITE_DSN` | `sqlite://synapse.db?mode=rwc` | SQLite DSN. Falls back to `SYNAPSE_LEDGER_DSN`, then the default path. |
| `SYNAPSE_LEDGER_POSTGRES_DSN` | — | Postgres DSN. Falls back to `SYNAPSE_LEDGER_DSN`. Required when `postgres` is in the backend list. |
| `SYNAPSE_LEDGER_DSN` | `sqlite://synapse.db?mode=rwc` | Legacy single-backend DSN (SQLite or Postgres). Prefer the per-backend vars above. |
| `SYNAPSE_LEDGER_PUBSUB_TOPIC` | — | Pub/Sub topic ID. Required when `pubsub` is in the backend list (`ledger-pubsub` feature). |
| `SYNAPSE_LEDGER_PUBSUB_PROJECT` | — | GCP project for Pub/Sub. Falls back to `VERTEX_PROJECT`. |
| `SYNAPSE_LEDGER_SNS_TOPIC_ARN` | — | SNS topic ARN. Required when `sns` is in the backend list (`ledger-sns` feature). |
| `SYNAPSE_LEDGER_SNS_REGION` | — | AWS region for SNS. Optional; the AWS default credential chain is used if absent. |
| `SYNAPSE_DEFAULT_TENANT` | `unattributed` | Tenant name used when `x-synapse-tenant` header is absent. |
| `SYNAPSE_REQUEST_TIMEOUT_SECS` | `120` | Time-to-first-chunk timeout in seconds. A leg that does not produce its first chunk within this window falls back to the next leg. |
| `SYNAPSE_STREAM_IDLE_TIMEOUT_SECS` | `60` | Maximum inter-chunk idle gap in seconds. A leg that stalls mid-stream for this long is terminated. |

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
| `synapse_ledger_errors_total` | Counter | `backend` | Per-sink write failures (e.g. `backend="pubsub"`). One sink failing does not stop the others. |

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

### Multi-sink fan-out

Multiple backends can run simultaneously. Every usage event is delivered to every configured sink concurrently. One sink failing never blocks the others; per-sink failures are logged and counted on `synapse_ledger_errors_total{backend=<name>}`.

Select backends with `SYNAPSE_LEDGER_BACKENDS` (comma-separated). The singular `SYNAPSE_LEDGER_BACKEND` is still accepted as a one-item fallback. When neither variable is set the default is `sqlite`.

```bash
# Fan-out to both Postgres and Pub/Sub
SYNAPSE_LEDGER_BACKENDS=postgres,pubsub
```

### Backends

| Backend | Cargo feature | Env vars | Notes |
|---------|--------------|----------|-------|
| SQLite | `ledger-sqlite` (default) | `SYNAPSE_LEDGER_SQLITE_DSN` (fallback: `SYNAPSE_LEDGER_DSN`, then `sqlite://synapse.db?mode=rwc`) | File created automatically. |
| Postgres | `ledger-postgres` | `SYNAPSE_LEDGER_POSTGRES_DSN` (fallback: `SYNAPSE_LEDGER_DSN`) | Requires a connection string. |
| GCP Pub/Sub | `ledger-pubsub` | `SYNAPSE_LEDGER_PUBSUB_TOPIC` (required), `SYNAPSE_LEDGER_PUBSUB_PROJECT` (fallback: `VERTEX_PROJECT`) | ADC auth; ordering key is `requestId`. |
| AWS SNS | `ledger-sns` | `SYNAPSE_LEDGER_SNS_TOPIC_ARN` (required), `SYNAPSE_LEDGER_SNS_REGION` (optional, else AWS default chain) | Standard AWS credential chain. |

SQLite is enabled by default. The cloud backends (`ledger-pubsub`, `ledger-sns`) are feature-gated and pull no cloud SDK unless explicitly enabled.

```bash
# Build with Pub/Sub support
cargo build --release --features ledger-pubsub

# Build with SNS support
cargo build --release --features ledger-sns

# Build with both cloud backends
cargo build --release --features "ledger-pubsub ledger-sns"
```

### Published event format (Pub/Sub and SNS)

Both cloud backends publish a talos-aligned JSON payload (`camelCase`; tenant as `namespace`; `type: "usage"`):

```json
{
  "namespace": "my-team",
  "requestId": "01929f3a-...",
  "timestamp": "2026-06-10T15:30:45Z",
  "type": "usage",
  "route": "gemini-pro",
  "provider": "vertex",
  "model": "gemini-3-pro",
  "lane": "standard",
  "inputTokens": 128,
  "outputTokens": 256,
  "costUsd": 0.00042,
  "status": "ok"
}
```

Every message carries attributes for subscription filtering: `namespace`, `requestId`, `type`, `provider`, `status`. Pub/Sub additionally sets `requestId` as the message ordering key.

### Schema

The single migration (`migrations/0001_usage_events.sql`) creates the `usage_events` table with columns for tenant, workspace, provider, model, input tokens, output tokens, cost, and timestamp.

---

## Building

### Cargo

```bash
# Default build (SQLite ledger)
cargo build --release

# Postgres ledger only
cargo build --release --no-default-features --features ledger-postgres

# SQLite + Pub/Sub fan-out
cargo build --release --features ledger-pubsub

# SQLite + SNS fan-out
cargo build --release --features ledger-sns

# All four backends
cargo build --release --features "ledger-pubsub ledger-sns ledger-postgres"
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

The test suite (66 tests) covers route resolution, fallback behaviour, lane detection, tenant attribution, config parsing, ledger writes, HTTP handler integration, streaming primitives, tool-call accumulation, first-chunk timeout fallback, and SSE serialisation.

---

## Limitations / roadmap

The following are **not** present in v1 and are planned for future releases:

- Authentication / API key enforcement on inbound requests.
- Rate limiting.
- Multi-region Vertex endpoint routing.
- Admin API for dynamic route reloading.

---

## Contributing

Contributions are welcome. See **[CONTRIBUTING.md](CONTRIBUTING.md)** for how to build, test, and submit changes. Commits must be signed off under the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`); contributions are licensed under AGPL-3.0. Please also read our **[Code of Conduct](CODE_OF_CONDUCT.md)**.

## Security

Found a vulnerability? **Do not open a public issue.** See **[SECURITY.md](SECURITY.md)** for private disclosure (email `raj@sustentabilitas.com` or a GitHub private advisory).

## License

Licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See **[LICENSE](LICENSE)**.
