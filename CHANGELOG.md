# Changelog

All notable changes to synapse-gateway are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] - 2026-06-16

### Fixed

- **Native Vertex lane dropped every SSE event under CRLF** — Vertex/gemini-3
  terminates `streamGenerateContent` (`?alt=sse`) events with `\r\n\r\n`, but the
  stream parser only split on `\n\n`, so no event boundary was ever found: the
  whole response was buffered and discarded, yielding empty content and 0 tokens
  on every native-lane call (schema-constrained structuring, `/extract`, the
  organisations resolver/verifier). The boundary scanner now matches both `\n\n`
  and `\r\n\r\n`.

## [0.5.0] - 2026-06-15

### Added

- **Per-leg Vertex region** — a route leg may now carry an optional `region`
  (`{ provider = "vertex", model = "gemini-3.1-pro-preview", region = "global" }`).
  The native Vertex lane uses it to pick both the API host and the
  `locations/{region}` path, falling back to the provider's configured region
  (env `VERTEX_LOCATION`) when unset — so a model can be pinned to the region
  that serves it without a process-wide env change.
- **Native Vertex thinking/token controls** — the native Vertex lane now emits
  `generationConfig.maxOutputTokens` from `ChatRequest.max_tokens` (previously
  dropped on this lane) and a new `VertexExt.thinking_config` passthrough for
  `generationConfig.thinkingConfig` (e.g. `{"thinkingLevel":"low"}` for Gemini 3,
  `{"thinkingBudget":N}` for Gemini 2.5). Without these, a thinking model under a
  `responseSchema` can spend its whole output budget reasoning and return an
  empty body.

### Fixed

- **Thinking parts excluded from content** — `parse_response` and
  `vertex_chunk_to_items` now skip Vertex parts flagged `"thought": true`, so a
  thinking model's reasoning no longer pollutes (or, under structured output,
  invalidates) the emitted text.

## [0.4.1] - 2026-06-15

### Fixed

- **Pub/Sub ledger** — pass `&str` to `PubsubLedger::connect` when resolving the project from `vertex_project_from_env`.

## [0.4.0] - 2026-06-15

### Added

- **`VERTEX_PROJECT_ID` env var** — preferred over legacy `VERTEX_PROJECT` for native Vertex, Vertex embeddings, and Pub/Sub ledger project resolution (`vertex_project_from_env`).

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

[Unreleased]: https://github.com/sustentabilitas/synapse-gateway/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.4.1
[0.4.0]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.4.0
[0.2.1]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.2.1
[0.2.0]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.2.0
[0.1.0]: https://github.com/sustentabilitas/synapse-gateway/releases/tag/v0.1.0
