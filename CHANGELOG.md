# Changelog

All notable changes to synapse-gateway are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

synapse-gateway is pre-1.0; nothing has been released yet. Everything below is the
initial feature set being readied for a first tagged release.

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

[Unreleased]: https://github.com/sustentabilitas/synapse-gateway/commits/main
