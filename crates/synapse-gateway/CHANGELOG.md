# Changelog

All notable changes to synapse-gateway are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.37] - 2026-09-26

### Added

- `SYNAPSE_PROVIDER_VALIDATION=lenient`: a route referencing a provider this
  process cannot build no longer has to stop the process. Unservable legs are
  dropped, each route keeps its remaining legs (so a route degrades to its
  fallback), and a route left with no legs disappears from the table — calling
  it is then a 404 rather than a crash. `strict` remains the default and is
  unchanged. Public API: `config::ProviderValidation`,
  `providers::unsatisfiable_providers`, `providers::Unsatisfiable`,
  `RouteTable::without_providers`, `EmbeddingRouteTable::without_providers`.

  Motivation: one `routes.toml` is commonly shared by a gateway and several
  in-process consumers. Adding a leg for the gateway then crashlooped every
  consumer that lacked that provider's credential — and a consumer running an
  older build crashed on the unrecognised provider id even with the credential
  present.
- `telemetry` module: `GatewayMetrics` (all gateway instruments, built from any
  OpenTelemetry `Meter`; `GatewayMetrics::noop()` records nothing),
  `seconds_histogram` and `SECONDS_BUCKETS`; with the `server` feature,
  `install` (Prometheus registry plus optional OTLP/HTTP reader),
  `MetricsExporter` and `metrics_router`. `CircuitBreaker::attach_metrics`
  records breaker and retry metrics for breakers built outside a `Catalog`.

### Changed

- Metrics are recorded with OpenTelemetry (opentelemetry-rust 0.32) instead of
  the `metrics` crate. Prometheus output on `SYNAPSE_METRICS_ADDR` keeps the
  same series names and labels; setting `OTEL_EXPORTER_OTLP_ENDPOINT` also
  pushes them over OTLP/HTTP (every 60 seconds by default, tagged with
  `service.name` from `OTEL_SERVICE_NAME`). Embedders pass
  `GatewayBuilder::metrics` (default no-op); `LedgerHandle::spawn_with_metrics`,
  `FanoutLedger::with_metrics`, `GuardEngine::with_metrics` and
  `Catalog::attach_metrics` are the opt-ins for code built outside the builder.
  `ledger::connect::build_store` now takes the metrics as a second argument,
  and `GenAiSpan::emit_metrics` takes `(&GatewayMetrics, secs)`.
- A global `metrics` recorder installed by a host process no longer receives
  gateway series. In-process consumers must pass `GatewayBuilder::metrics` to
  keep recording them.
- Duration metrics (`*_duration_seconds`) are now histograms with second-based
  `_bucket` series instead of summaries; queries on `{quantile=...}` must move
  to `histogram_quantile(...)` over `_bucket`.
- Each metric is capped at 2000 label combinations (OpenTelemetry SDK default);
  overflow is folded into an `otel_metric_overflow="true"` series.
- `synapse_passthrough_total` from the Gemini passthrough now carries
  `provider="vertex"`, matching the Jev passthrough's label set.

### Fixed

- Grafana dashboard latency panels (`histogram_quantile` over
  `*_duration_seconds_bucket`) now show data; the previous exporter emitted
  summaries, so they were empty.

## [0.5.36] - 2026-09-23

### Fixed

- Jev hybrid extraction: a request carrying native-Vertex features (a `vertex`
  block) together with `extract` on a route that has no `vertex` leg now gets
  `400 Bad Request` before any upstream call, instead of silently degrading to
  an empty `answers` map. Mirrors the plain native-Vertex lane, which has
  always 4xx'd in that situation.
## [0.5.35] - 2026-09-23

### Added

- Jev hybrid extraction: a `jev` extension block may carry an `extract` spec
  (`candidates` gated by `noul` questions, `floor`, `prompt` template with
  `{{text}}`, `response_schema`). The gateway judges candidates with Jev and
  runs one schema-pinned chat extraction per survivor on the route's chat
  legs (per candidate, sequential). The response gains a `jev` block
  (`answers`, `survivors`, `degraded`) beside the OpenAI envelope;
  `choices[0].message.content` is a JSON map of candidate key → extraction.
  When every `typesafe` leg fails retryably, extraction runs ungated over all
  candidates with `degraded: true`. BREAKING (library): `Gateway::chat` now
  returns `ChatOutcome` (`Plain` | `Hybrid`) instead of `Completion`.
- TypeSafe System One (Jev) passthrough: `POST /typesafe/v1/systemone` forwards
  `{state, questions}` bodies verbatim to TypeSafe's API and meters usage from
  the response's `usage` block. Jev answers typed questions (choice / score /
  noul) with structured decisions and has no OpenAI-shaped equivalent, so no
  translation to `/v1/chat/completions` is attempted. Enabled when
  `TYPESAFE_API_KEY` is set; `TYPESAFE_BASE_URL` overrides the hosted endpoint.
- Jev chat lane: a route alias can list `typesafe` legs alongside chat legs,
  and a chat request carrying a `jev` extension block (typed `questions`,
  optional `state` override) is answered by TypeSafe System One — the
  decision arrives as JSON-encoded `answers` in the message content. When
  every `typesafe` leg fails retryably (429/408/5xx/transport) the remaining
  legs fall back to a normal chat completion (native-Vertex fallback when a
  `vertex` extension block is present); non-retryable TypeSafe errors abort.
  With `stream: true` the decision is a single SSE chunk. `typesafe` legs
  without a `jev` block are a clear 400, and referencing `typesafe` without
  `TYPESAFE_API_KEY` fails fast at startup.

### Changed

- The passthrough usage meter now records the lane's provider and op
  (`vertex`/`chat` for Gemini, `typesafe`/`systemone` for Jev) and understands
  both usage shapes: Vertex `usageMetadata` and TypeSafe
  `usage.{input,output}_tokens`.

## [0.5.34] - 2026-09-10

### Changed

- A2A static seed soft-skips agents whose `card_url` is unreachable or invalid
  after retries (warn + continue). Prevents CrashLoop deadlock when a peer
  agent (e.g. ploutonion) is still starting.

## [0.5.33] - 2026-09-03

## [0.5.32] - 2026-09-03

### Added

- `ai_task_type` on every ledger row: the kind of work the gateway performed,
  resolved from the `x-synapse-ai-task-type` header when supplied, else inferred
  from the request's route alias via the new `config/ai_task_types.toml` table
  (keyed by task type, so several aliases can share one), else `"simple"`.
  The file is optional and its path is set by `SYNAPSE_AI_TASK_TYPES_PATH`; an
  alias mapped to two task types fails at startup. Recorded as the
  `ai_task_type` column (SQLite and Postgres, backfilled on existing tables) and
  as `aiTaskType` on published Pub/Sub and SNS usage events. Library callers set
  `RequestCtx::ai_task_type`.

### Changed

- The streaming and Gemini-passthrough usage meters now take a single
  `Attribution` argument instead of six positional `Option<String>` parameters,
  where a swapped pair would compile cleanly and mis-attribute usage.

## [0.5.31] - 2026-09-03

### Added

- Optional `x-synapse-user-task-type` request header: a free-form caller-supplied
  classification of the work a request serves. Accepted on `/v1/chat/completions`
  (buffered and streaming), `/v1/embeddings`, and the Gemini-native passthrough,
  and propagated to the cost ledger as the `user_task_type` column (SQLite and
  Postgres, backfilled on existing tables) and as `userTaskType` on published
  Pub/Sub and SNS usage events. Kept off metrics and `gen_ai.*` spans to bound
  label cardinality. Library callers set `RequestCtx::user_task_type`.

## [0.5.30] - 2026-08-11

## [0.5.29] - 2026-08-11

## [0.5.27] - 2026-08-11

## [0.5.26] - 2026-08-11

## [0.5.25] - 2026-08-11

## [0.5.24] - 2026-08-07

## [0.5.23] - 2026-08-06

## [0.5.22] - 2026-07-30

### Fixed

- **`vertex.thinking_config` selects the native Vertex lane** — the extension
  was already parsed and applied on native requests, but lane classification
  only keyed off `cached_content` / `response_schema` / `gs://` media URIs.
  Requests that only set `thinking_config` (e.g. `thinkingBudget: 0` for
  cheap autocomplete) stayed on the standard lane, so the config was ignored.

## [0.5.21] - 2026-07-30

## [0.5.20] - 2026-07-30

### Changed

- **Default `config/{routes,pricing,guardrails}.toml`** — align with the
  shared cluster catalog: Gemini 3.5/3.6 pins + multi-region legs, cortex
  aliases (`graph-*`, `nl-*`, `vector-embed`), wine2o2/talos routes,
  `browser-agent` on `gemini-3.6-flash`, `scraped` policy, and
  `invisible_text` removed from `guardrails.default`.

## [0.5.19] - 2026-07-29

## [0.5.18] - 2026-07-29

### Added

- **Multi-region Vertex endpoint routing** — location ids `us` and `eu` resolve
  to `https://aiplatform.{us|eu}.rep.googleapis.com` with `locations/{us|eu}`
  (alongside existing `global` and single-region hosts). Shared by the native
  Vertex lane, genai Vertex provider, and Vertex embeddings.

## [0.5.17] - 2026-07-20

## [0.5.16] - 2026-07-20

### Added

- **Thread / message attribution** — `RequestCtx` and ledger `UsageEntry` /
  `UsageEvent` now carry optional `thread` / `message` (`threadId` /
  `messageId` on the published event), stamped from `x-synapse-thread` /
  `x-synapse-message` over HTTP. When `request_id` is unset, the gateway
  falls back to `message` so a chat turn correlates with usage rows.

## [0.5.15] - 2026-07-20

## [0.5.14] - 2026-07-18

## [0.5.12] - 2026-07-18

## [0.5.11] - 2026-07-16

## [0.5.10] - 2026-07-16

## [0.5.9] - 2026-07-08

## [0.5.7] - 2026-06-23

## [0.5.5] - 2026-06-23

## [0.5.4] - 2026-06-19

## [0.5.3] - 2026-06-19

### Added

- **Input guardrails** — policy config (TOML), scanner registry with bundle aliases,
  `GuardEngine` (compile, input extraction, decision, metrics), and `ContentBlocked`
  with an OpenAI-shaped 400 response.

## [0.5.2] - 2026-06-16

### Fixed

- **Native Vertex ADC token expiry (`401 ACCESS_TOKEN_EXPIRED`)** — `VertexAuth::
  from_adc` pinned a fixed 50-minute lifetime on each fetched bearer, but the GKE
  metadata server serves the same token until minutes before its ~1-hour expiry,
  so a token fetched late in its life was cached past expiry → Vertex 401s on the
  native lane for a ~40-minute window each cycle. The ADC credential is now built
  once and reused; `google-cloud-auth` refreshes internally and `access_token()`
  returns a currently-valid token, with only a short synapse-side TTL on top.

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
