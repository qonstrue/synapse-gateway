# Embeddings — configuration

synapse-gateway exposes an OpenAI-compatible **`POST /v1/embeddings`** that routes a
model *alias* through dimension-pinned fallback legs to a **Vertex AI** (`:predict`)
or **OpenAI-compatible** embedding model — with the same per-leg fallback, tenant
attribution, and cost ledger as the chat endpoint.

## 1. Define embedding routes

Embedding aliases live in the **same `routes.toml`** as chat routes, under a
separate `[embeddings."<alias>"]` table. Each alias declares a `dimensions` and an
ordered list of `legs`:

```toml
# config/routes.toml

[embeddings."default-embed"]
dimensions = 768
legs = [
  { provider = "vertex", model = "text-embedding-004" },
  { provider = "openai", model = "text-embedding-3-small" },   # fallback, pinned to 768
]
```

- **`dimensions` is mandatory** and is *pinned on every leg* — the gateway sends
  Vertex `outputDimensionality` and OpenAI `dimensions` so **all legs return the same
  vector length**. This is what makes fallback safe for a vector index (a Vertex→OpenAI
  failover can't suddenly change 768→1536 and corrupt your store).
- Legs are tried **in order**; a leg whose provider has no credentials configured is
  skipped, and a leg that errors falls through to the next.
- Legacy fixed-dimension models that can't reduce output (e.g. `textembedding-gecko`,
  `text-embedding-ada-002`) can't join a pinned alias.

## 2. Provider credentials (env)

Build only the providers your embedding aliases reference. Missing credentials for a
referenced provider is a **fail-fast boot error**.

| Provider | Env | Notes |
|----------|-----|-------|
| `vertex` | `VERTEX_PROJECT` (required), `VERTEX_LOCATION` (default `global`) | Auth via ADC → **Workload Identity** on GKE — no key. Reuses the same project/location as the native chat lane. |
| `openai` | `OPENAI_API_KEY` (required), `OPENAI_BASE_URL` (default `https://api.openai.com/v1`) | Any OpenAI-compatible embeddings endpoint works via `OPENAI_BASE_URL`. |

## 3. Pricing & cost

Embedding cost is recorded to the ledger (input tokens only). Add a price per
`provider:model` in `pricing.toml` (USD per 1M tokens):

```toml
# config/pricing.toml
[ "vertex:text-embedding-004" ]
input = 0.025
output = 0.0
```

Unlike chat, an **unpriced** embedding model does **not** cost `0` — it falls back to
a default so usage is never silently free:

| Env | Default | Effect |
|-----|---------|--------|
| `SYNAPSE_EMBED_DEFAULT_INPUT_PRICE_PER_MTOK` | `0.10` | USD per 1M input tokens used when no `pricing.toml` key matches. |

## 4. Tenant / workspace attribution

Send the same headers as the chat endpoint; usage is attributed and written to the
cost ledger as a `UsageEvent` with `op = "embedding"`, `output_tokens = 0`:

| Header | Maps to |
|--------|---------|
| `x-synapse-tenant` | ledger `namespace` (falls back to `SYNAPSE_DEFAULT_TENANT`) |
| `x-synapse-workspace` | ledger `workspace` |

## 5. Request / response

```bash
curl -s http://localhost:8080/v1/embeddings \
  -H 'content-type: application/json' \
  -H 'x-synapse-tenant: acme' \
  -d '{ "input": ["hello world", "second chunk"], "model": "default-embed" }'
```

```jsonc
{
  "object": "list",
  "data": [
    { "object": "embedding", "index": 0, "embedding": [/* 768 floats */] },
    { "object": "embedding", "index": 1, "embedding": [/* 768 floats */] }
  ],
  "model": "default-embed",
  "usage": { "prompt_tokens": 7, "total_tokens": 7 }
}
```

- `input` accepts a string or an array of strings; large arrays are split into
  per-provider batches (Vertex 250 / OpenAI 2048) and reassembled in `index` order.
- An optional request `dimensions` is allowed only if it **equals** the alias's
  declared dimension (otherwise `400`) — the alias owns index-safety.

## 6. Embedding as a library

The engine is usable in-process (no HTTP server):

```rust
let resp = gateway.embed(
    EmbeddingRequest { input: EmbeddingInput::Many(vec!["a".into(), "b".into()]), model: "default-embed".into(), dimensions: None },
    RequestCtx { tenant: Some("acme".into()), workspace: None, request_id: None },
).await?;
```

## 7. Observability

Per-request metrics `synapse_embeddings_total` and `synapse_embedding_duration_seconds`
(labels `route` / `model` / `provider`) are emitted on `:9090/metrics`; token usage and
cost flow to the ledger. The Grafana dashboard has an **Embeddings** row — see
[monitoring.md](monitoring.md).
