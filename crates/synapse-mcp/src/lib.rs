//! `synapse-mcp`: MCP gateway crate (scaffold — Task 1 of the gateway plan).
//!
//! rmcp header injection: identity headers are set **PER-CONNECTION**, not
//! per-call; the gateway must **rebuild the upstream client (a new
//! `StreamableHttpClientTransport` from a new `StreamableHttpClientTransportConfig`)
//! whenever the `ContextStore` overlay changes or expires** — it cannot mutate
//! headers on an already-connected client.
//!
//! Evidence, verified against rmcp 2.2.0 (modelcontextprotocol/rust-sdk,
//! commit `5195776`, tagged `rmcp-v2.2.0` in `crates/rmcp/Cargo.toml`):
//!
//! - `StreamableHttpClientTransportConfig::custom_headers` is a
//!   `HashMap<HeaderName, HeaderValue>` field, set once via the
//!   `.custom_headers(...)` builder method **before** the transport is
//!   constructed (`StreamableHttpClientTransport::from_config` /
//!   `StreamableHttpClientWorker::new`).
//!   (`crates/rmcp/src/transport/streamable_http_client.rs`, struct
//!   `StreamableHttpClientTransportConfig`, field `custom_headers` and its
//!   builder `custom_headers()`.)
//! - `StreamableHttpClientWorker::run` reads `config.custom_headers` exactly
//!   once at session start (cloned into a local `protocol_headers` binding)
//!   and reuses that same clone for **every** subsequent `post_message`,
//!   `get_stream`, and `delete_session` call for the lifetime of the worker —
//!   including after transparent session re-initialization
//!   (`perform_reinitialization` re-derives `new_protocol_headers` from the
//!   same `config.custom_headers`, not from anything call-scoped).
//! - The reqwest `StreamableHttpClient` impl
//!   (`crates/rmcp/src/transport/common/reqwest/streamable_http_client.rs`,
//!   `apply_custom_headers`) applies whatever `custom_headers` map it is
//!   handed to the outgoing `reqwest::RequestBuilder` — it has no
//!   knowledge of, or hook for, a per-call override.
//! - `CallToolRequestParams` (`crates/rmcp/src/model.rs`) — the only
//!   per-call request shape reachable from `Peer::call_tool` /
//!   `RunningService::call_tool` — has fields `meta`, `name`, `arguments`,
//!   `task` only. There is no header field and no other call-site hook to
//!   carry a header through to the transport.
//!
//! Net effect: a single `StreamableHttpClientTransport` instance echoes the
//! same header value for the entire session, no matter how many tool calls
//! are made on it (proven by
//! `tests/rmcp_spike.rs::headers_are_fixed_per_connection_not_per_call`,
//! which calls the same client twice and gets the same value both times,
//! then shows a second, differently-configured client echoes a different
//! value). This gates a later task: the gateway's per-forwarded-request
//! identity injection (mirroring `transform/inject.rs`'s `ContextStore`
//! resolve-per-request contract) requires either (a) rebuilding the upstream
//! `StreamableHttpClientTransport`/client whenever the resolved
//! `ResolvedContext` overlay changes, or (b) maintaining a small pool of
//! per-identity clients keyed by the resolved header values — but it
//! categorically cannot rely on mutating headers on one long-lived client.

pub mod registry;

pub use registry::{McpRegistry, RegisteredUpstream};
