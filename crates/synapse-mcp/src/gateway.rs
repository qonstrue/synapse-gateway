//! Transparent `/mcp/{server}` gateway: routes a sandbox MCP session to a
//! registered upstream MCP server, injecting the session's tenant identity.
//!
//! ## Design
//!
//! Per the Task-1 finding (see `crate::lib` top doc-comment), rmcp's
//! `StreamableHttpClientTransportConfig::custom_headers` is fixed at
//! transport construction — there is no per-call header hook. So the
//! upstream identity (`x-org-id`/`x-workspace-id`/`x-user-id`) must be baked
//! into a dedicated upstream client, and that client must be rebuilt
//! whenever the bound identity changes.
//!
//! The gateway is itself exposed to the sandbox as a single MCP *server*
//! (`GatewayHandler`) mounted once via rmcp's `StreamableHttpService`, and
//! reused for every `/mcp/{server}` request rather than one service per
//! name — `axum`'s plain `.route()` (not `.nest_service()`) leaves the
//! request's `Uri` untouched, so `GatewayHandler` recovers which upstream
//! `{server}` a call targets by reading the literal HTTP request path back
//! out of `RequestContext::extensions` (rmcp stores the original
//! `http::request::Parts` there per request — see `tests/rmcp_spike.rs`).
//! `GatewayHandler` delegates `list_tools`/`call_tool` to a cached upstream
//! client for that server; this is the "`ServerHandler`-delegate" fallback
//! the brief allows in place of a fully transparent byte-for-byte JSON-RPC
//! passthrough (which rmcp's typed API does not expose a hook for).
//!
//! Every delegated call re-resolves the registry and the bound identity
//! before touching the network, so an unknown/expired server or an unbound
//! identity fails closed without ever dialing upstream.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::Request;
use axum::response::IntoResponse;
use axum::Router;
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ErrorData as McpError, RoleClient, ServerHandler, ServiceExt};
use synapse_context::{ContextStore, ResolvedContext};
use tokio::sync::Mutex as AsyncMutex;
use tower::Service;

use crate::registry::McpRegistry;

type UpstreamClient = rmcp::service::RunningService<RoleClient, ClientInfo>;

/// A single "context key → forwarded header" injection rule. The downstream
/// broker supplies the concrete rules (e.g. `org` → `x-org-id`); this crate
/// has no built-in notion of what identity looks like.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct IdentityHeaderRule {
    /// Key looked up in the `ContextStore` overlay (`ResolvedContext::get`).
    pub context_key: String,
    /// Forwarded HTTP header name set from that value.
    pub header: String,
    /// If `true` and `context_key` is absent from the resolved context, the
    /// call fails closed instead of forwarding without that header.
    #[serde(default)]
    pub required: bool,
}

/// Config-driven identity injection for the gateway. An empty `inject` list
/// is a valid configuration: it means no identity is injected into forwarded
/// requests at all (the fail-closed guarantee only applies to `required`
/// rules — there is nothing to fail closed on if there are no rules).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct McpGatewayConfig {
    #[serde(default)]
    pub inject: Vec<IdentityHeaderRule>,
}

/// The resolved `(header, value)` pairs for one forwarded call, computed by
/// applying `McpGatewayConfig::inject` against a `ResolvedContext`. Sorted by
/// header name so the cache-key fingerprint is stable regardless of the
/// order rules are declared in config. This is both the source of the
/// forwarded headers and the `ClientCache` key: two calls that resolve to
/// the same pairs share a client, any difference forces a rebuild — so a
/// different identity can never reuse another's client.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ResolvedInjection(Vec<(String, String)>);

impl ResolvedInjection {
    /// Resolve every rule in `config.inject` against `ctx`. Fail closed
    /// (`GatewayError::Unbound`) if a `required` rule's `context_key` is
    /// absent; an absent *optional* rule's header is simply omitted from the
    /// result, and the call still proceeds. Never reads anything from a
    /// client-supplied header — there is no code path here that touches the
    /// inbound sandbox request's headers at all, so a client-supplied
    /// spoofed header cannot leak upstream.
    fn resolve(config: &McpGatewayConfig, ctx: &ResolvedContext) -> Result<Self, GatewayError> {
        let mut pairs = Vec::with_capacity(config.inject.len());
        for rule in &config.inject {
            match ctx.get(&rule.context_key) {
                Some(value) => pairs.push((rule.header.clone(), value.to_string())),
                None if rule.required => {
                    return Err(GatewayError::Unbound(rule.context_key.clone()))
                }
                None => {}
            }
        }
        pairs.sort();
        Ok(Self(pairs))
    }

    /// Build the forwarded headers from the resolved pairs.
    fn header_map(&self) -> Result<HashMap<HeaderName, HeaderValue>, GatewayError> {
        let mut headers = HashMap::with_capacity(self.0.len());
        for (name, value) in &self.0 {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                GatewayError::Internal(format!("invalid header name '{name}': {e}"))
            })?;
            headers.insert(header_name, to_header_value(value)?);
        }
        Ok(headers)
    }
}

fn to_header_value(value: &str) -> Result<HeaderValue, GatewayError> {
    HeaderValue::from_str(value)
        .map_err(|e| GatewayError::Internal(format!("invalid identity header value: {e}")))
}

/// Gateway-side failure modes, each mapped to an MCP protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayError {
    /// `registry.resolve(name)` returned `None` (unregistered or TTL-expired).
    UnknownServer(String),
    /// A `required` injection rule's `context_key` is missing from the
    /// resolved context.
    Unbound(String),
    /// Anything else (header construction, upstream connect failure, ...).
    Internal(String),
}

impl GatewayError {
    fn into_mcp_error(self) -> McpError {
        match self {
            GatewayError::UnknownServer(name) => McpError::resource_not_found(
                format!("unknown or expired mcp server '{name}'"),
                None,
            ),
            GatewayError::Unbound(key) => McpError::invalid_request(
                format!("context not bound: missing identity key '{key}'"),
                None,
            ),
            GatewayError::Internal(message) => {
                // The detailed message (upstream URL, raw transport error
                // text, ...) is operational detail for us, not for the
                // sandbox-facing caller — log it here and hand back a
                // generic message so no host/URL/transport internals leak
                // across the trust boundary.
                tracing::warn!(error = %message, "gateway internal error; returning generic error to caller");
                McpError::internal_error("upstream MCP server unavailable", None)
            }
        }
    }
}

/// Cache of live upstream rmcp clients. Keyed by `(server name, resolved
/// injection)` per the Task-1 finding (headers are per-connection, not
/// per-call); on insert, any other cached client for the same server name
/// under a *different* resolved injection is evicted (dropped, which cancels
/// it) — the bound `ContextStore` overlay is process-wide and single-active,
/// so at most one resolved injection is ever valid for a given server at a
/// time.
///
/// Each entry also remembers the `url` it was built against, so a registry
/// hot-swap (`McpRegistry::register` replacing an existing name's URL) is
/// picked up even when the resolved injection is unchanged — see
/// `get_or_build`.
struct CachedClient {
    client: Arc<UpstreamClient>,
    url: String,
}

#[derive(Default)]
struct ClientCache {
    entries: AsyncMutex<HashMap<(String, ResolvedInjection), CachedClient>>,
}

impl ClientCache {
    /// `registry` is threaded through so every call can also sweep entries
    /// for servers that have since been deregistered or TTL-expired — see
    /// `sweep_deregistered`. This is the only eviction path for those
    /// entries: nothing else in the gateway proactively closes a dropped
    /// server's cached connections.
    async fn get_or_build(
        &self,
        registry: &McpRegistry,
        server: &str,
        url: &str,
        injection: &ResolvedInjection,
    ) -> Result<Arc<UpstreamClient>, GatewayError> {
        let key = (server.to_string(), injection.clone());
        {
            let mut guard = self.entries.lock().await;
            if let Some(entry) = guard.get(&key) {
                if entry.url == url {
                    let client = entry.client.clone();
                    sweep_deregistered(&mut guard, registry);
                    return Ok(client);
                }
                // The resolved injection is unchanged but the registry now
                // resolves this server to a different URL (hot-swap) — fall
                // through and rebuild against the fresh URL rather than
                // serving the stale connection.
            }
        }
        let client = Arc::new(build_upstream_client(url, injection).await?);
        let mut guard = self.entries.lock().await;
        // Evict stale injections for this server; keep other servers' entries.
        guard.retain(|(name, inj), _| name != server || inj == injection);
        guard.insert(
            key,
            CachedClient {
                client: client.clone(),
                url: url.to_string(),
            },
        );
        sweep_deregistered(&mut guard, registry);
        Ok(client)
    }
}

/// Drop every cache entry whose server name no longer resolves in the
/// registry (deregistered, or TTL-expired — `McpRegistry::resolve` returns
/// `None` for both and lazily removes expired entries itself). Run on every
/// `get_or_build` call so a long-lived gateway bounds its live upstream
/// connections to currently-registered servers, instead of accumulating a
/// dead `Arc<UpstreamClient>` (live TCP + rmcp task) per churned server
/// name forever. Dropping the `CachedClient` is sufficient to cancel its
/// connection — rmcp's client shuts down on drop.
fn sweep_deregistered(
    entries: &mut HashMap<(String, ResolvedInjection), CachedClient>,
    registry: &McpRegistry,
) {
    entries.retain(|(name, _), _| registry.resolve(name).is_some());
}

async fn build_upstream_client(
    url: &str,
    injection: &ResolvedInjection,
) -> Result<UpstreamClient, GatewayError> {
    let headers = injection.header_map()?;
    let config =
        StreamableHttpClientTransportConfig::with_uri(url.to_string()).custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_config(config);
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        rmcp::model::Implementation::new("synapse-mcp-gateway", env!("CARGO_PKG_VERSION")),
    );
    client_info.serve(transport).await.map_err(|e| {
        GatewayError::Internal(format!("connecting upstream mcp server at '{url}': {e}"))
    })
}

/// The core dispatch: resolve the registry, fail closed on any `required`
/// injection rule whose key is absent, then get-or-build the cached upstream
/// client. No network call happens until both checks pass.
async fn resolve_upstream(
    registry: &McpRegistry,
    context: &ContextStore,
    clients: &ClientCache,
    config: &McpGatewayConfig,
    server: &str,
) -> Result<Arc<UpstreamClient>, GatewayError> {
    let url = registry
        .resolve(server)
        .ok_or_else(|| GatewayError::UnknownServer(server.to_string()))?;
    let injection = ResolvedInjection::resolve(config, &context.resolve())?;
    clients
        .get_or_build(registry, server, &url, &injection)
        .await
}

/// Recover which `/mcp/{server}` this call targets from the raw HTTP
/// request rmcp stashes in `RequestContext::extensions` (see module doc).
fn server_name_from_context(context: &RequestContext<RoleServer>) -> Result<String, GatewayError> {
    context
        .extensions
        .get::<http::request::Parts>()
        .map(|parts| parts.uri.path())
        .and_then(|path| path.strip_prefix("/mcp/"))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::Internal("could not determine target mcp server from request path".into())
        })
}

/// The MCP server the sandbox connects to. Delegates `list_tools`/
/// `call_tool` to whichever upstream the request's `/mcp/{server}` path
/// names, with the configured identity headers injected on the gateway's own
/// upstream connection.
#[derive(Clone)]
struct GatewayHandler {
    registry: Arc<McpRegistry>,
    context: Arc<ContextStore>,
    clients: Arc<ClientCache>,
    config: Arc<McpGatewayConfig>,
}

impl GatewayHandler {
    async fn upstream_for(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<Arc<UpstreamClient>, McpError> {
        let server = server_name_from_context(context).map_err(GatewayError::into_mcp_error)?;
        resolve_upstream(
            &self.registry,
            &self.context,
            &self.clients,
            &self.config,
            &server,
        )
        .await
        .map_err(GatewayError::into_mcp_error)
    }
}

impl ServerHandler for GatewayHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let client = self.upstream_for(&context).await?;
        client
            .list_tools(request)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let client = self.upstream_for(&context).await?;
        client
            .call_tool(request)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

/// Build the sandbox-facing gateway router. Every `/mcp/{server}` request is
/// served by one shared `StreamableHttpService<GatewayHandler>` (so MCP
/// session state persists across requests within a session); `server` is
/// recovered per-request from the literal path, not from routing state.
///
/// `config` supplies the context-key → header injection rules; this crate
/// has no built-in notion of tenant identity, so an empty `config.inject`
/// forwards calls with no injected headers at all (see `McpGatewayConfig`).
pub fn mcp_gateway_router(
    registry: Arc<McpRegistry>,
    context: Arc<ContextStore>,
    config: Arc<McpGatewayConfig>,
) -> Router {
    let handler = GatewayHandler {
        registry,
        context,
        clients: Arc::new(ClientCache::default()),
        config,
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        // Defaults already restrict inbound `Host` to loopback — do not
        // hand-roll Host/Origin validation (DNS-rebinding protection is
        // free in rmcp >= 1.4 via `allowed_hosts`).
        StreamableHttpServerConfig::default(),
    );

    Router::new().route(
        "/mcp/{server}",
        axum::routing::any(move |req: Request| {
            let mut svc = service.clone();
            async move {
                match Service::call(&mut svc, req).await {
                    Ok(response) => response.map(axum::body::Body::new).into_response(),
                    Err(never) => match never {},
                }
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        CallToolRequestMethod, ContentBlock, ServerCapabilities as SrvCaps, ServerInfo as SrvInfo,
        Tool,
    };
    use rmcp::transport::streamable_http_server::StreamableHttpService as SrvHttpService;
    use std::net::SocketAddr;

    /// Generic config reproducing the old hardcoded org/workspace/user
    /// contract: three `required` rules, so a downstream broker wanting the
    /// old fail-closed-on-any-missing-key behavior gets it back via config,
    /// not code.
    fn three_required_rules() -> McpGatewayConfig {
        McpGatewayConfig {
            inject: vec![
                IdentityHeaderRule {
                    context_key: "org".to_string(),
                    header: "x-org-id".to_string(),
                    required: true,
                },
                IdentityHeaderRule {
                    context_key: "workspace".to_string(),
                    header: "x-workspace-id".to_string(),
                    required: true,
                },
                IdentityHeaderRule {
                    context_key: "user".to_string(),
                    header: "x-user-id".to_string(),
                    required: true,
                },
            ],
        }
    }

    fn bound_context() -> ContextStore {
        ContextStore::new(HashMap::from([
            ("org".to_string(), "acme".to_string()),
            ("workspace".to_string(), "ws1".to_string()),
            ("user".to_string(), "u1".to_string()),
        ]))
    }

    // ---- Fail-closed unit tests: no network involved -----------------

    #[tokio::test]
    async fn unknown_server_errors_without_contacting_upstream() {
        let registry = McpRegistry::new();
        let context = bound_context();
        let clients = ClientCache::default();
        let config = three_required_rules();

        let err = resolve_upstream(&registry, &context, &clients, &config, "missing")
            .await
            .expect_err("unregistered server must error");

        assert_eq!(err, GatewayError::UnknownServer("missing".to_string()));
    }

    #[tokio::test]
    async fn expired_server_errors_without_contacting_upstream() {
        let registry = McpRegistry::new();
        registry.register(
            "alpha".to_string(),
            "http://127.0.0.1:1".to_string(),
            Some(std::time::Duration::from_secs(0)),
        );
        // TTL of 0 is already expired relative to "now" on the next resolve.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let context = bound_context();
        let clients = ClientCache::default();
        let config = three_required_rules();

        let err = resolve_upstream(&registry, &context, &clients, &config, "alpha")
            .await
            .expect_err("expired server must error");

        assert_eq!(err, GatewayError::UnknownServer("alpha".to_string()));
    }

    #[tokio::test]
    async fn empty_overlay_fails_closed_without_contacting_upstream() {
        let registry = McpRegistry::new();
        // Points at a closed/unroutable port: if the gateway dialed upstream
        // before checking identity, this would hang or fail as a connection
        // error instead of returning `Unbound` immediately.
        registry.register("alpha".to_string(), "http://127.0.0.1:1".to_string(), None);
        let context = ContextStore::new(HashMap::new());
        let clients = ClientCache::default();
        let config = three_required_rules();

        let err = resolve_upstream(&registry, &context, &clients, &config, "alpha")
            .await
            .expect_err("unbound context must fail closed");

        assert!(matches!(err, GatewayError::Unbound(_)));
    }

    #[tokio::test]
    async fn partially_bound_overlay_fails_closed() {
        let registry = McpRegistry::new();
        registry.register("alpha".to_string(), "http://127.0.0.1:1".to_string(), None);
        // "user" missing.
        let context = ContextStore::new(HashMap::from([
            ("org".to_string(), "acme".to_string()),
            ("workspace".to_string(), "ws1".to_string()),
        ]));
        let clients = ClientCache::default();
        let config = three_required_rules();

        let err = resolve_upstream(&registry, &context, &clients, &config, "alpha")
            .await
            .expect_err("partially bound context must fail closed");

        assert_eq!(err, GatewayError::Unbound("user".to_string()));
    }

    #[tokio::test]
    async fn optional_rule_with_absent_key_is_simply_omitted() {
        // A rule with `required: false` whose context_key is absent must not
        // fail closed — the header is just omitted and the call proceeds.
        let upstream_addr = spawn_echo_upstream().await;
        let registry = Arc::new(McpRegistry::new());
        registry.register(
            "echo-server".to_string(),
            format!("http://{upstream_addr}/mcp"),
            None,
        );

        let config = Arc::new(McpGatewayConfig {
            inject: vec![
                IdentityHeaderRule {
                    context_key: "org".to_string(),
                    header: "x-org-id".to_string(),
                    required: true,
                },
                IdentityHeaderRule {
                    context_key: "nickname".to_string(),
                    header: "x-nickname".to_string(),
                    required: false,
                },
            ],
        });
        // "nickname" is deliberately absent from the bound overlay.
        let context = Arc::new(ContextStore::new(HashMap::new()));
        context.push(
            HashMap::from([("org".to_string(), "acme-corp".to_string())]),
            None,
        );

        let gateway_addr = spawn_gateway(registry, context, config).await;
        let client = connect_sandbox_client(gateway_addr, "echo-server", HashMap::new()).await;

        let echoed = call_echo(&client).await;

        assert_eq!(
            echoed, "acme-corp",
            "the call must still succeed and forward the required header \
             even though the optional rule's key was absent"
        );
        client.cancel().await.ok();
    }

    // ---- DNS-rebinding protection (rmcp default allowed_hosts) -------

    /// A request with a non-loopback `Host` must be rejected by rmcp's
    /// built-in DNS-rebinding guard (`StreamableHttpServerConfig::default()`
    /// allows only `localhost`/`127.0.0.1`/`::1`), NOT reach the gateway
    /// handler. Proves we rely on the default rather than disabling it.
    #[tokio::test]
    async fn spoofed_host_is_rejected_by_dns_rebinding_guard() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let registry = Arc::new(McpRegistry::new());
        let context = Arc::new(bound_context());
        let config = Arc::new(three_required_rules());
        let router = mcp_gateway_router(registry, context, config);

        let req = Request::builder()
            .method("POST")
            .uri("/mcp/platform")
            .header("host", "evil.example.com")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            ))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    // ---- Happy-path identity-injection proof, end to end -------------

    /// Single-tool in-process MCP server: `echo` returns the caller's
    /// `x-org-id` header value (empty string if absent). Mirrors
    /// `tests/rmcp_spike.rs::EchoServer`.
    #[derive(Clone, Default)]
    struct EchoUpstream;

    impl ServerHandler for EchoUpstream {
        fn get_info(&self) -> SrvInfo {
            SrvInfo::new(SrvCaps::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "echo",
                "echoes the caller's x-org-id header back as tool result text",
                serde_json::Map::new(),
            )]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            if request.name != "echo" {
                return Err(McpError::method_not_found::<CallToolRequestMethod>());
            }
            let org_id = context
                .extensions
                .get::<http::request::Parts>()
                .and_then(|parts| parts.headers.get("x-org-id"))
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            Ok(CallToolResult::success(vec![ContentBlock::text(org_id)]))
        }
    }

    async fn spawn_echo_upstream() -> SocketAddr {
        let service = SrvHttpService::new(
            || Ok(EchoUpstream),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let router: Router = Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("axum::serve exited");
        });
        addr
    }

    async fn spawn_gateway(
        registry: Arc<McpRegistry>,
        context: Arc<ContextStore>,
        config: Arc<McpGatewayConfig>,
    ) -> SocketAddr {
        let router = mcp_gateway_router(registry, context, config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("axum::serve exited");
        });
        addr
    }

    async fn connect_sandbox_client(
        gateway_addr: SocketAddr,
        server_name: &str,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> rmcp::service::RunningService<RoleClient, ClientInfo> {
        let config = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://{gateway_addr}/mcp/{server_name}"
        ))
        .custom_headers(custom_headers);
        let transport = StreamableHttpClientTransport::from_config(config);
        let client_info = ClientInfo::new(
            ClientCapabilities::default(),
            rmcp::model::Implementation::new("sandbox-test-client", "0.0.1"),
        );
        client_info
            .serve(transport)
            .await
            .expect("sandbox client connect")
    }

    async fn call_echo(client: &rmcp::service::RunningService<RoleClient, ClientInfo>) -> String {
        let result = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()))
            .await
            .expect("call_tool through gateway");
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|text| text.text.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn bound_identity_reaches_upstream_through_the_gateway_router() {
        let upstream_addr = spawn_echo_upstream().await;

        let registry = Arc::new(McpRegistry::new());
        registry.register(
            "echo-server".to_string(),
            format!("http://{upstream_addr}/mcp"),
            None,
        );

        let context = Arc::new(ContextStore::new(HashMap::new()));
        context.push(
            HashMap::from([
                ("org".to_string(), "acme-corp".to_string()),
                ("workspace".to_string(), "ws-1".to_string()),
                ("user".to_string(), "u-42".to_string()),
            ]),
            None,
        );

        let config = Arc::new(three_required_rules());
        let gateway_addr = spawn_gateway(registry, context, config).await;
        let client = connect_sandbox_client(gateway_addr, "echo-server", HashMap::new()).await;

        let echoed = call_echo(&client).await;

        assert_eq!(echoed, "acme-corp");
        client.cancel().await.ok();
    }

    #[tokio::test]
    async fn client_cache_reuses_same_identity_but_rebuilds_on_change() {
        let upstream_addr = spawn_echo_upstream().await;
        let url = format!("http://{upstream_addr}/mcp");
        let clients = ClientCache::default();
        let registry = McpRegistry::new();
        registry.register("alpha".to_string(), url.clone(), None);

        let injection_a = ResolvedInjection(vec![
            ("x-org-id".to_string(), "org-a".to_string()),
            ("x-user-id".to_string(), "u".to_string()),
            ("x-workspace-id".to_string(), "ws".to_string()),
        ]);
        let client_a_first = clients
            .get_or_build(&registry, "alpha", &url, &injection_a)
            .await
            .expect("build client for injection_a");

        // Same resolved injection, same server: reused, not rebuilt.
        let client_a_second = clients
            .get_or_build(&registry, "alpha", &url, &injection_a)
            .await
            .expect("resolve cached client for injection_a");
        assert!(
            Arc::ptr_eq(&client_a_first, &client_a_second),
            "unchanged resolved injection must reuse the cached client, not rebuild it"
        );

        let injection_b = ResolvedInjection(vec![
            ("x-org-id".to_string(), "org-b".to_string()),
            ("x-user-id".to_string(), "u".to_string()),
            ("x-workspace-id".to_string(), "ws".to_string()),
        ]);
        let client_b = clients
            .get_or_build(&registry, "alpha", &url, &injection_b)
            .await
            .expect("build client for injection_b");

        assert!(
            !Arc::ptr_eq(&client_a_first, &client_b),
            "changed resolved injection must rebuild the upstream client, not reuse the old one"
        );
        let guard = clients.entries.lock().await;
        assert_eq!(
            guard.len(),
            1,
            "the stale injection_a entry for 'alpha' must be evicted, not accumulated"
        );
        assert!(guard.contains_key(&("alpha".to_string(), injection_b)));
    }

    #[tokio::test]
    async fn different_resolved_identities_get_different_cached_clients() {
        // End-to-end version of the cache-isolation guarantee: two gateway
        // calls under different bound identities must never share an
        // upstream client, proven by two live sandbox connections routed
        // through the same running gateway with different `ContextStore`
        // overlays.
        let upstream_addr = spawn_echo_upstream().await;
        let registry = Arc::new(McpRegistry::new());
        registry.register(
            "echo-server".to_string(),
            format!("http://{upstream_addr}/mcp"),
            None,
        );
        let config = Arc::new(three_required_rules());

        let context_a = Arc::new(ContextStore::new(HashMap::new()));
        context_a.push(
            HashMap::from([
                ("org".to_string(), "acme-corp".to_string()),
                ("workspace".to_string(), "ws-1".to_string()),
                ("user".to_string(), "u-42".to_string()),
            ]),
            None,
        );
        let gateway_a_addr = spawn_gateway(registry.clone(), context_a, config.clone()).await;
        let client_a = connect_sandbox_client(gateway_a_addr, "echo-server", HashMap::new()).await;
        let echoed_a = call_echo(&client_a).await;
        assert_eq!(echoed_a, "acme-corp");

        let context_b = Arc::new(ContextStore::new(HashMap::new()));
        context_b.push(
            HashMap::from([
                ("org".to_string(), "beta-corp".to_string()),
                ("workspace".to_string(), "ws-2".to_string()),
                ("user".to_string(), "u-7".to_string()),
            ]),
            None,
        );
        let gateway_b_addr = spawn_gateway(registry, context_b, config).await;
        let client_b = connect_sandbox_client(gateway_b_addr, "echo-server", HashMap::new()).await;
        let echoed_b = call_echo(&client_b).await;

        assert_eq!(
            echoed_b, "beta-corp",
            "a different resolved identity must reach upstream with its own headers, \
             never reusing the other identity's cached client"
        );

        client_a.cancel().await.ok();
        client_b.cancel().await.ok();
    }

    /// Second in-process upstream, distinguishable from `EchoUpstream`:
    /// always returns a fixed marker regardless of headers, so a test can
    /// tell whether a call actually landed on it vs. `EchoUpstream`.
    #[derive(Clone, Default)]
    struct MarkerUpstream;

    impl ServerHandler for MarkerUpstream {
        fn get_info(&self) -> SrvInfo {
            SrvInfo::new(SrvCaps::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "echo",
                "always returns a fixed marker, ignoring headers",
                serde_json::Map::new(),
            )]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            if request.name != "echo" {
                return Err(McpError::method_not_found::<CallToolRequestMethod>());
            }
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "upstream-b-marker".to_string(),
            )]))
        }
    }

    async fn spawn_marker_upstream() -> SocketAddr {
        let service = SrvHttpService::new(
            || Ok(MarkerUpstream),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let router: Router = Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("axum::serve exited");
        });
        addr
    }

    #[tokio::test]
    async fn registry_url_hot_swap_rebuilds_the_cached_upstream_client() {
        // Task-1/Finding-1 regression: `ClientCache` used to key purely on
        // (server, identity), so a registry hot-swap to a new URL under an
        // unchanged identity kept serving the stale cached client forever.
        let upstream_a_addr = spawn_echo_upstream().await;
        let upstream_b_addr = spawn_marker_upstream().await;

        let registry = Arc::new(McpRegistry::new());
        registry.register(
            "alpha".to_string(),
            format!("http://{upstream_a_addr}/mcp"),
            None,
        );

        let context = Arc::new(ContextStore::new(HashMap::new()));
        context.push(
            HashMap::from([
                ("org".to_string(), "acme-corp".to_string()),
                ("workspace".to_string(), "ws-1".to_string()),
                ("user".to_string(), "u-42".to_string()),
            ]),
            None,
        );

        let config = Arc::new(three_required_rules());
        let gateway_addr = spawn_gateway(registry.clone(), context, config).await;
        let client = connect_sandbox_client(gateway_addr, "alpha", HashMap::new()).await;

        let first = call_echo(&client).await;
        assert_eq!(
            first, "acme-corp",
            "first call must hit upstream A and cache against it"
        );

        // Hot-swap the registry entry to a DIFFERENT in-process server under
        // the same name; the bound identity does not change.
        registry.register(
            "alpha".to_string(),
            format!("http://{upstream_b_addr}/mcp"),
            None,
        );

        let second = call_echo(&client).await;
        assert_eq!(
            second, "upstream-b-marker",
            "next call under the same identity must rebuild against the hot-swapped url and hit upstream B"
        );

        client.cancel().await.ok();
    }

    #[tokio::test]
    async fn client_supplied_identity_header_is_ignored() {
        let upstream_addr = spawn_echo_upstream().await;

        let registry = Arc::new(McpRegistry::new());
        registry.register(
            "echo-server".to_string(),
            format!("http://{upstream_addr}/mcp"),
            None,
        );

        let context = Arc::new(ContextStore::new(HashMap::new()));
        context.push(
            HashMap::from([
                ("org".to_string(), "acme-corp".to_string()),
                ("workspace".to_string(), "ws-1".to_string()),
                ("user".to_string(), "u-42".to_string()),
            ]),
            None,
        );

        let config = Arc::new(three_required_rules());
        let gateway_addr = spawn_gateway(registry, context, config).await;
        // The sandbox tries to spoof its own identity header when talking to
        // the gateway; the gateway must never forward it.
        let spoofed_headers = HashMap::from([(
            HeaderName::from_static("x-org-id"),
            HeaderValue::from_static("attacker"),
        )]);
        let client = connect_sandbox_client(gateway_addr, "echo-server", spoofed_headers).await;

        let echoed = call_echo(&client).await;

        assert_eq!(
            echoed, "acme-corp",
            "spoofed client header must not reach upstream"
        );
        client.cancel().await.ok();
    }

    #[tokio::test]
    async fn unknown_server_through_router_returns_error_without_upstream() {
        // No upstream is ever spawned in this test — if the gateway
        // contacted one before checking the registry, connecting would hang
        // or fail differently instead of returning a clean MCP error.
        let registry = Arc::new(McpRegistry::new());
        let context = Arc::new(ContextStore::new(HashMap::from([
            ("org".to_string(), "acme".to_string()),
            ("workspace".to_string(), "ws1".to_string()),
            ("user".to_string(), "u1".to_string()),
        ])));

        let config = Arc::new(three_required_rules());
        let gateway_addr = spawn_gateway(registry, context, config).await;
        let client = connect_sandbox_client(gateway_addr, "does-not-exist", HashMap::new()).await;

        let err = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()))
            .await
            .expect_err("unknown server must surface as an error");

        assert!(err.to_string().to_lowercase().contains("unknown"));
        client.cancel().await.ok();
    }

    // ---- Cache eviction for deregistered/expired servers -------------

    /// Regression for the unbounded-cache finding: a server's cached
    /// upstream client must not outlive its registration. Registers
    /// "alpha", caches a client for it, deregisters "alpha", then registers
    /// and resolves a *different* server "beta" — the resolve for "beta"
    /// must sweep the now-orphaned "alpha" entry out of the cache.
    #[tokio::test]
    async fn deregistered_server_client_is_evicted_on_next_sweep() {
        let alpha_upstream_addr = spawn_echo_upstream().await;
        let beta_upstream_addr = spawn_echo_upstream().await;

        let registry = McpRegistry::new();
        registry.register(
            "alpha".to_string(),
            format!("http://{alpha_upstream_addr}/mcp"),
            None,
        );
        let context = bound_context();
        let clients = ClientCache::default();
        let config = three_required_rules();

        resolve_upstream(&registry, &context, &clients, &config, "alpha")
            .await
            .expect("cache a client for alpha");
        {
            let guard = clients.entries.lock().await;
            assert!(
                guard.keys().any(|(name, _)| name == "alpha"),
                "alpha's client must be cached before deregistration"
            );
        }

        registry.deregister("alpha");
        registry.register(
            "beta".to_string(),
            format!("http://{beta_upstream_addr}/mcp"),
            None,
        );

        resolve_upstream(&registry, &context, &clients, &config, "beta")
            .await
            .expect("cache a client for beta; this call also sweeps stale entries");

        let guard = clients.entries.lock().await;
        assert!(
            !guard.keys().any(|(name, _)| name == "alpha"),
            "alpha's cached client must be evicted once its registration is gone"
        );
        assert!(
            guard.keys().any(|(name, _)| name == "beta"),
            "beta's client must still be cached"
        );
    }

    /// Same as above but for TTL expiry rather than explicit deregistration:
    /// `McpRegistry::resolve` returns `None` for an expired entry just as it
    /// does for a deregistered one, so the same sweep must evict it too.
    #[tokio::test]
    async fn ttl_expired_server_client_is_evicted_on_next_sweep() {
        let alpha_upstream_addr = spawn_echo_upstream().await;
        let beta_upstream_addr = spawn_echo_upstream().await;

        let registry = McpRegistry::new();
        registry.register(
            "alpha".to_string(),
            format!("http://{alpha_upstream_addr}/mcp"),
            Some(std::time::Duration::from_millis(20)),
        );
        let context = bound_context();
        let clients = ClientCache::default();
        let config = three_required_rules();

        resolve_upstream(&registry, &context, &clients, &config, "alpha")
            .await
            .expect("cache a client for alpha before it expires");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        registry.register(
            "beta".to_string(),
            format!("http://{beta_upstream_addr}/mcp"),
            None,
        );
        resolve_upstream(&registry, &context, &clients, &config, "beta")
            .await
            .expect("cache a client for beta; this call also sweeps expired entries");

        let guard = clients.entries.lock().await;
        assert!(
            !guard.keys().any(|(name, _)| name == "alpha"),
            "alpha's cached client must be evicted once its TTL expires"
        );
        assert!(
            guard.keys().any(|(name, _)| name == "beta"),
            "beta's client must still be cached"
        );
    }
}
