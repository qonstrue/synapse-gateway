# Changelog


## 0.2.8


## 0.2.7

- Fix upstream forwarding after JSON body transforms (`inject` / `wrap` on `body`): strip stale client `Content-Length` and `Transfer-Encoding` so reqwest sends the full re-serialized body (fixes truncated JSON at upstream when callers like Python urllib set length for the pre-transform payload).

## 0.2.6

## 0.2.4


## 0.2.3


## 0.2.2


## 0.2.1

## 0.2.0

- Rule engine: context sources (`static` + `env` + admin push/clear), `require_context` gate, and a full transform pipeline (`inject`, `wrap`, `error_remap`) on request and response.
- Three concurrent listeners: data (`addr`), admin (`admin_addr`), metrics (`metrics_addr`) served via `tokio::try_join!` with graceful SIGTERM drain.
- OTel metrics exported in Prometheus format: `synapse_proxy_requests_total`, `synapse_proxy_request_duration_seconds`, `synapse_proxy_upstream_errors_total`, `synapse_proxy_transform_errors_total`.
- Custom transform extension API: `ProxyBuilder::request_transform` / `response_transform`.
- MCP/A2A-compatible: agent tool calls are plain `[[routes]]` using `inject` into `params.*` body paths plus streaming passthrough.
- Broker-parity integration test: wrap envelope + error_remap end-to-end.

## 0.1.0

- Initial release: config-driven passthrough routing (longest `path_prefix`
  match, optional prefix strip), static per-route header injection, streaming
  responses, `/healthz/{liveness,readiness}`, and graceful SIGTERM drain.
