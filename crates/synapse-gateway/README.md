# synapse-gateway

[![crates.io](https://img.shields.io/crates/v/synapse-gateway.svg)](https://crates.io/crates/synapse-gateway)
[![Docker Hub](https://img.shields.io/docker/v/sustentabilitas/synapse-gateway?logo=docker&label=docker)](https://hub.docker.com/r/sustentabilitas/synapse-gateway)
[![License: MPL-2.0](https://img.shields.io/badge/License-MPL--2.0-brightgreen.svg)](https://github.com/sustentabilitas/synapse-gateway/blob/main/LICENSE)
[![CI](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/sustentabilitas/synapse-gateway/actions/workflows/ci.yml)

**English** · [Español](https://github.com/sustentabilitas/synapse-gateway/blob/main/crates/synapse-gateway/README.es-ES.md)

synapse-gateway is an open-source LLM gateway written in Rust. Your clients send standard
OpenAI `POST /v1/chat/completions` requests; the gateway routes each one through a
config-driven fallback chain of providers (Vertex AI, OpenAI, Qwen and self-hosted
OpenAI-compatible servers) and records what it cost in a per-tenant ledger. Requests travel
on one of three lanes: the standard OpenAI-compatible lane, a native Vertex AI lane that keeps
context caching, `gs://` media and strict response schemas, and a Jev lane for typed
decisions from TypeSafe System One. Streaming, tool calling, embeddings, input guardrails and
`synapse_*` metrics are built in.

**Full documentation:** https://synapse-gateway.readthedocs.io/en/latest/docs/overview/introduction/

## Install

```bash
# Docker image (linux/amd64)
docker pull sustentabilitas/synapse-gateway

# Binary
cargo install synapse-gateway
```

To embed the gateway in a Rust service, add the library without its default features and
call `Gateway::chat()` in-process (the library crate is named `synapse`):

```toml
[dependencies]
synapse-gateway = { version = "2", default-features = false }
```

See [Installation](https://synapse-gateway.readthedocs.io/en/latest/docs/get-started/installation/)
for Cargo features and ledger backends, and
[Embedding Synapse as a library](https://synapse-gateway.readthedocs.io/en/latest/docs/guides/embedding-as-library/).

## Example

Save a route, its prices and a guardrails policy:

```toml
# config/routes.toml
[routes."gemini-flash"]
legs = [
  { provider = "vertex", model = "gemini-3.5-flash-lite", region = "us" },
]
```

```toml
# config/pricing.toml
"vertex:gemini-3.5-flash-lite" = { input = 0.30, output = 2.50 }
```

```toml
# config/guardrails.toml
[guardrails.default]
scanners = ["prompt_injection", "secrets"]
```

Start the gateway with Application Default Credentials for Vertex AI, then send a request:

```bash
gcloud auth application-default login
VERTEX_PROJECT_ID=my-gcp-project synapse-gateway   # API on :8080, metrics on :9090

# In a second terminal:
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-synapse-tenant: my-team" \
  -d '{"model": "gemini-flash", "messages": [{"role": "user", "content": "Hello!"}]}'
```

The [quickstart](https://synapse-gateway.readthedocs.io/en/latest/docs/get-started/quickstart/)
adds streaming, fallback to OpenAI, native Vertex requests and the cost ledger.

## Learn more

- [Architecture](https://synapse-gateway.readthedocs.io/en/latest/docs/overview/architecture/): lanes and fallback chains
- [Routes](https://synapse-gateway.readthedocs.io/en/latest/docs/configuration/routes/), [Jev 2D harness router (`config/harnesses.toml`)](https://synapse-gateway.readthedocs.io/en/latest/docs/guides/jev-harness-router/), and [environment variables](https://synapse-gateway.readthedocs.io/en/latest/docs/configuration/environment-variables/)
- [HTTP API reference](https://synapse-gateway.readthedocs.io/en/latest/docs/reference/http-api/)
- [Metrics](https://synapse-gateway.readthedocs.io/en/latest/docs/operating/metrics/)
- [Limitations and roadmap](https://synapse-gateway.readthedocs.io/en/latest/docs/reference/limitations-roadmap/)

## Contributing

Contributions are welcome. See **[CONTRIBUTING.md](https://github.com/sustentabilitas/synapse-gateway/blob/main/CONTRIBUTING.md)** for how to build, test, and submit changes. Commits must be signed off under the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`); contributions are licensed under MPL-2.0. Please also read our **[Code of Conduct](https://github.com/sustentabilitas/synapse-gateway/blob/main/CODE_OF_CONDUCT.md)**.

## Security

Found a vulnerability? **Do not open a public issue.** See **[SECURITY.md](https://github.com/sustentabilitas/synapse-gateway/blob/main/SECURITY.md)** for private disclosure (email `raj@sustentabilitas.com` or a GitHub private advisory).

## License

Licensed under the **Mozilla Public License 2.0** (MPL-2.0), which welcomes commercial use and asks that changes to Synapse's own files are shared back. See **[LICENSE](https://github.com/sustentabilitas/synapse-gateway/blob/main/LICENSE)** and the [licence note](https://github.com/sustentabilitas/synapse-gateway#license).
