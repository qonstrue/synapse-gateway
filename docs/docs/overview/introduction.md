---
sidebar_position: 1
title: What is Synapse?
description: Synapse is an open-source Rust LLM gateway that is OpenAI-compatible on the outside and keeps native Vertex AI and Jev routing on the inside.
---

Synapse is an open-source LLM gateway written in Rust. Your clients send standard OpenAI
`POST /v1/chat/completions` requests; Synapse routes each one through a config-driven
fallback chain of providers and records what it cost. Unlike generic OpenAI-compatible
proxies, it keeps a native Vertex AI lane, so Vertex-only features survive the trip, and a
lane for TypeSafe System One (Jev), a hosted service that answers typed questions with
structured decisions. You run Synapse as a single binary or Docker image, or embed it in a
Rust service as a library. See [Installation](../get-started/installation.md).

Synapse is a Cargo workspace. The crates are:

- **`synapse-gateway`**: the LLM gateway, and the subject of most of this documentation.
- **`synapse-proxy`**: a config-driven reverse-proxy sidecar with path-prefix routing,
  context injection, request and response transforms, and streaming passthrough. See
  [synapse-proxy](../synapse-family/proxy/overview.md).
- **`synapse-a2a`**: an agent-to-agent (A2A) registry with admin registration and public
  discovery endpoints, served by the gateway binary. See
  [synapse-a2a](../synapse-family/a2a/overview.md).
- **`synapse-mcp`**: an on-demand MCP gateway that routes Streamable HTTP traffic per
  server and injects per-session tenant identity. See
  [synapse-mcp](../synapse-family/mcp/overview.md).
- **`synapse-context`**: the shared context store (a static base plus a TTL overlay) used
  by `synapse-proxy` and `synapse-mcp`. See
  [Workspace crates](../internals/workspace-crates.md#synapse-context).

## Why is Synapse different?

Synapse was built after evaluating
[`litellm-rs`](https://github.com/majiayu000/litellm-rs) and the general "put an
OpenAI-compatible proxy in front of everything" approach. That approach reaches Gemini
through a generic OpenAI-shaped adapter, which throws away context caching, Cloud Storage
(`gs://`) media and strict response schemas. Synapse keeps those by sending such requests
down a dedicated native Vertex lane, while every other request rides the standard
OpenAI-compatible lane. You get multi-provider routing and Vertex's native features, not
one or the other.

It also uses TypeSafe System One (**Jev**) across the request lifecycle: a guardrail policy
can ask Jev to semantically classify inputs before any routing or provider call, the Jev
router asks Jev how demanding each request is to pick a model tier and reasoning effort, and
the Jev lane answers typed questions or gates hybrid extractions. Because Synapse is one
small Rust codebase rather than a framework, the routing, fallback, guardrails, ledger and
observability code is yours to read, run and embed.

## When to use Synapse

Use Synapse when:

- You depend on Vertex-only features such as context caching, `gs://` media URIs or strict
  `responseSchema`, and still want an OpenAI-compatible API with fallback to other providers.
- You want each request routed to the right model and reasoning effort for its difficulty,
  instead of hard-coding one model per use case.
- You need per-tenant cost accounting: every request is attributed to a tenant and priced
  into a ledger you own.
- You want to embed an LLM gateway inside a Rust service rather than run a separate process.

Synapse is not a good fit when:

- You need inbound authentication or rate limiting today. Synapse has neither yet; run it
  behind your own API gateway, ingress or service mesh. See
  [Security](../operating/security.md#callers-arent-authenticated) and
  [Limitations and roadmap](../reference/limitations-roadmap.md).
- You want a hosted SaaS. Synapse is self-hosted software.

## Key features

Native capabilities:

- **Native Vertex lane**: `cachedContent`, `gs://` media URIs, strict `responseSchema` and
  `thinking_config`, sent directly to Vertex AI's `:streamGenerateContent` endpoint. See
  [Native Vertex lane](architecture.md#native-vertex-lane) and the
  [native Vertex guide](../guides/native-vertex.md).
- **Native tool calling**: tools work on the standard and native Vertex lanes; on the native
  Vertex lane,
  `tool_choice` is honoured through Vertex `toolConfig`. See
  [Streaming and tool calling](../guides/streaming-and-tools.md#tool-calling).
- **Jev lane**: typed decisions from Jev, plus hybrid judge-then-extract in one call. See
  [Jev lane](architecture.md#jev-lane) and the [Jev lane guide](../guides/jev-lane.md).
- **Jev router**: per-request tier and reasoning effort, mapped to Vertex `thinkingBudget`
  on native Vertex legs and reported in `x-synapse-routing` and `x-synapse-tier` response
  headers. See the [Jev router guide](../guides/jev-router.md) and
  [Jev routes](../configuration/routes.md#jev-routes).
- **Jev 2D harness router (`config/harnesses.toml`)**: routes Universal Harness Protocol
  (`POST /v1/responses`) turns across a 2D `(harness × model)` grid—selecting both the agent
  runtime (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) and the model difficulty
  tier in a single Jev call, pinning the harness base across `previous_response_id`
  session continuations, and re-scoring model tiers per turn. See the
  [Jev 2D harness router guide](../guides/jev-harness-router.md).
- **Native Gemini & Jev passthrough**: forwards `@google/genai` SDK calls
  (`POST /v1beta/models/{model}:{action}`) and TypeSafe System One calls
  (`POST /typesafe/v1/systemone`) with Vertex fallback chains, input guardrails and
  per-tenant cost metering. See the [HTTP API reference](../reference/http-api.md).
- **Real streaming**: Synapse always streams from upstream, so `stream: true` clients get
  token-by-token server-sent events; non-streaming clients get the buffered result and, on
  the standard lane, keep the full fallback chain. See
  [Streaming and tool calling](../guides/streaming-and-tools.md).
- **Embeddable**: run the `synapse-gateway` binary, or depend on the library crate and call
  `Gateway::chat()` in-process. See
  [Embedding Synapse as a library](../guides/embedding-as-library.md).

Compatibility and operations:

- **OpenAI-compatible API**: existing OpenAI SDKs work unchanged, including
  `POST /v1/embeddings`. See the [HTTP API reference](../reference/http-api.md) and the
  [embeddings guide](../guides/embeddings.md).
- **Multi-provider fallback**: Vertex AI, OpenAI, Qwen (DashScope), and self-hosted vLLM,
  Ollama or TGI through the `oai_compat` provider. See
  [Providers](../configuration/providers.md) and
  [Fallback chains](../guides/fallback-chains.md).
- **Per-tenant cost ledger**: token usage and cost per request, written to SQLite or
  Postgres and optionally fanned out to Google Cloud Pub/Sub and AWS SNS. See the
  [cost ledger guide](../guides/cost-ledger.md) and
  [Tenant attribution](../guides/tenant-attribution.md).
- **Observability**: OpenTelemetry `synapse_*` metrics, served in Prometheus format and
  optionally pushed over OTLP. See [Metrics](../operating/metrics.md), the
  [metrics catalogue](../reference/metrics-catalogue.md) and the
  [Grafana dashboard](../operating/grafana-dashboard.md).
- **Input and output guardrails**: named policies combining deterministic in-process scanners
  (prompt injection, secrets, PII, tenant-isolation bypass, data exfiltration and more),
  in-place `[REDACTED:...]` masking, optional Jev semantic classification, streaming output
  holdback scanning, and `observe` (dry-run) mode. See
  [Guardrails policy](../configuration/guardrails-policy.md).
