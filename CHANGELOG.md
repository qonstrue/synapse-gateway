# Changelog

All notable changes to synapse-gateway are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-06-14

### Added

- **Multimodal message content** — OpenAI-style `message.content` arrays (text, `image_url`, `inline_data`) are mapped to genai and native Vertex parts, enabling PDF/image inputs for embedded callers such as wine2o2.
- **`Catalog::from_map`** — construct a provider catalog from pre-built providers (tests and embedders).
- **`response_format` in genai lane** — `json_object` and `json_schema` are forwarded through genai `ChatOptions` for structured output.

## [0.2.0] - 2026-06-11

### Added

- **Embeddings endpoint** — OpenAI-compatible `POST /v1/embeddings` routing a model alias through dimension-pinned fallback legs to native **Vertex AI** (`:predict`) or **OpenAI-compatible** embedding models. The per-alias declared `dimensions` is pinned on every leg (Vertex `outputDimensionality` / OpenAI `dimensions`) so fallback never changes vector length. Tenant/workspace attribution and a cost element are recorded to the ledger (`op:"embedding"`, `output_tokens:0`; unpriced embeddings default to `$0.10`/1M via `SYNAPSE_EMBED_DEFAULT_INPUT_PRICE_PER_MTOK`). Adds the in-process `Gateway::embed()` method and `synapse_embeddings_total` / `synapse_embedding_duration_seconds` metrics.
- **Configurable Vertex region** — `VERTEX_LOCATION` (default `global`) selects the Vertex endpoint for the native lane.

## [0.1.0] - 2026-06-10

### Added

- **OpenAI-compatible gateway** — `POST /v1/chat/completions` and `GET /v1/models`, so existing OpenAI SDKs work unchanged.
- **Dual-lane routing** — a *standard lane* via the [`genai`](https://crates.io/crates/genai) crate (OpenAI, Qwen/DashScope, and any OpenAI-compatible endpoint) and a *native Vertex AI lane* (raw Vertex REST) preserving `cachedContent` context caching, `gs://` Cloud Storage media URIs, and strict `responseSchema` constrained decoding.
- **Config-driven fallback chains** with per-leg circuit breakers and retry classification.
- **Real token-by-token streaming** — every request streams from upstream internally; `stream: true` returns OpenAI-compatible SSE (`chat.completion.chunk` … `data: [DONE]`), while non-streaming clients get the same response buffered (retaining full chain fallback, including on mid-stream failures).
- **Tool / function calling on both lanes** — OpenAI `tools` / `tool_choice` in; `tool_calls` + `finish_reason: "tool_calls"` out; streamed as indexed deltas, reassembled for buffered responses.
- **First-chunk and idle stream timeouts** with fallback (`SYNAPSE_REQUEST_TIMEOUT_SECS`, `SYNAPSE_STREAM_IDLE_TIMEOUT_SECS`).
- **Multi-sink cost ledger** — a `FanoutLedger` records each usage event to every configured sink concurrently; backends: SQLite, Postgres, Google Cloud Pub/Sub, and AWS SNS (`SYNAPSE_LEDGER_BACKENDS`). Cloud backends publish a talos-aligned `UsageEvent` and are feature-gated.
- **Per-tenant cost accounting** — a static pricing table plus the durable ledger, attributed via `x-synapse-tenant` / `x-synapse-workspace`.
- **Observability** — `gen_ai.*` OpenTelemetry span attributes and a Prometheus pull endpoint on every request.
- **Embeddable library** — `synapse::gateway::Gateway` (builder + in-process `chat()` / `chat_stream()`); the axum HTTP server and Prometheus exporter are behind a default-on `server` feature, so the engine can be embedded with `default-features = false`. See `examples/embed.rs`.

[Unreleased]: https://github.com/sustentabilitas/synapse-gateway/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.2.1
[0.2.0]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.2.0
[0.1.0]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.1.0
