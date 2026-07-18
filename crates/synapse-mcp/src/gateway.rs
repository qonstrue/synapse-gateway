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
use synapse_proxy::context::{ContextStore, ResolvedContext};
use tokio::sync::Mutex as AsyncMutex;
use tower::Service;

use crate::registry::McpRegistry;

type UpstreamClient = rmcp::service::RunningService<RoleClient, ClientInfo>;

/// The three identity values every forwarded call must carry. All three or
/// none — a partially-bound overlay is treated the same as an empty one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    org: String,
    workspace: String,
    user: String,
}

impl Identity {
    /// Fail closed if any of `org`/`workspace`/`user` is absent from the
    /// resolved context (mirrors `transform/inject.rs`'s per-key contract).
    fn from_context(ctx: &ResolvedContext) -> Result<Self, GatewayError> {
        let get = |key: &'static str| ctx.get(key).map(str::to_string).ok_or(GatewayError::Unbound(key));
        Ok(Self {
            org: get("org")?,
            workspace: get("workspace")?,
            user: get("user")?,
        })
    }

    /// Build the exact three forwarded headers from the bound identity.
    /// Never reads anything from a client-supplied header — there is no
    /// code path here that touches the inbound sandbox request's headers at
    /// all, so a client-supplied `x-org-id` etc. cannot leak upstream.
    fn header_map(&self) -> Result<HashMap<HeaderName, HeaderValue>, GatewayError> {
        let mut headers = HashMap::with_capacity(3);
        headers.insert(HeaderName::from_static("x-org-id"), to_header_value(&self.org)?);
        headers.insert(
            HeaderName::from_static("x-workspace-id"),
            to_header_value(&self.workspace)?,
        );
        headers.insert(HeaderName::from_static("x-user-id"), to_header_value(&self.user)?);
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
    /// The resolved context is missing one of `org`/`workspace`/`user`.
    Unbound(&'static str),
    /// Anything else (header construction, upstream connect failure, ...).
    Internal(String),
}

impl GatewayError {
    fn into_mcp_error(self) -> McpError {
        match self {
            GatewayError::UnknownServer(name) => {
                McpError::resource_not_found(format!("unknown or expired mcp server '{name}'"), None)
            }
            GatewayError::Unbound(key) => McpError::invalid_request(
                format!("context not bound: missing identity key '{key}'"),
                None,
            ),
            GatewayError::Internal(message) => McpError::internal_error(message, None),
        }
    }
}

/// Cache of live upstream rmcp clients. Keyed by `(server name, identity)`
/// per the Task-1 finding (headers are per-connection, not per-call); on
/// insert, any other cached client for the same server name under a
/// *different* identity is evicted (dropped, which cancels it) — the bound
/// `ContextStore` overlay is process-wide and single-active, so at most one
/// identity is ever valid for a given server at a time.
#[derive(Default)]
struct ClientCache {
    entries: AsyncMutex<HashMap<(String, Identity), Arc<UpstreamClient>>>,
}

impl ClientCache {
    async fn get_or_build(
        &self,
        server: &str,
        url: &str,
        identity: &Identity,
    ) -> Result<Arc<UpstreamClient>, GatewayError> {
        let key = (server.to_string(), identity.clone());
        {
            let guard = self.entries.lock().await;
            if let Some(client) = guard.get(&key) {
                return Ok(client.clone());
            }
        }
        let client = Arc::new(build_upstream_client(url, identity).await?);
        let mut guard = self.entries.lock().await;
        // Evict stale identities for this server; keep other servers' entries.
        guard.retain(|(name, id), _| name != server || id == identity);
        guard.insert(key, client.clone());
        Ok(client)
    }
}

async fn build_upstream_client(url: &str, identity: &Identity) -> Result<UpstreamClient, GatewayError> {
    let headers = identity.header_map()?;
    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string()).custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_config(config);
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        rmcp::model::Implementation::new("synapse-mcp-gateway", env!("CARGO_PKG_VERSION")),
    );
    client_info
        .serve(transport)
        .await
        .map_err(|e| GatewayError::Internal(format!("connecting upstream mcp server at '{url}': {e}")))
}

/// The core dispatch: resolve the registry, fail closed on an unbound
/// identity, then get-or-build the cached upstream client. No network call
/// happens until both checks pass.
async fn resolve_upstream(
    registry: &McpRegistry,
    context: &ContextStore,
    clients: &ClientCache,
    server: &str,
) -> Result<Arc<UpstreamClient>, GatewayError> {
    let url = registry
        .resolve(server)
        .ok_or_else(|| GatewayError::UnknownServer(server.to_string()))?;
    let identity = Identity::from_context(&context.resolve())?;
    clients.get_or_build(server, &url, &identity).await
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
/// names, with the tenant identity injected on the gateway's own upstream
/// connection.
#[derive(Clone)]
struct GatewayHandler {
    registry: Arc<McpRegistry>,
    context: Arc<ContextStore>,
    clients: Arc<ClientCache>,
}

impl GatewayHandler {
    async fn upstream_for(&self, context: &RequestContext<RoleServer>) -> Result<Arc<UpstreamClient>, McpError> {
        let server = server_name_from_context(context).map_err(GatewayError::into_mcp_error)?;
        resolve_upstream(&self.registry, &self.context, &self.clients, &server)
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
pub fn mcp_gateway_router(registry: Arc<McpRegistry>, context: Arc<ContextStore>) -> Router {
    let handler = GatewayHandler {
        registry,
        context,
        clients: Arc::new(ClientCache::default()),
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
        CallToolRequestMethod, ContentBlock, ServerCapabilities as SrvCaps, ServerInfo as SrvInfo, Tool,
    };
    use rmcp::transport::streamable_http_server::StreamableHttpService as SrvHttpService;
    use std::net::SocketAddr;

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

        let err = resolve_upstream(&registry, &context, &clients, "missing")
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

        let err = resolve_upstream(&registry, &context, &clients, "alpha")
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

        let err = resolve_upstream(&registry, &context, &clients, "alpha")
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

        let err = resolve_upstream(&registry, &context, &clients, "alpha")
            .await
            .expect_err("partially bound context must fail closed");

        assert_eq!(err, GatewayError::Unbound("user"));
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
            axum::serve(listener, router).await.expect("axum::serve exited");
        });
        addr
    }

    async fn spawn_gateway(registry: Arc<McpRegistry>, context: Arc<ContextStore>) -> SocketAddr {
        let router = mcp_gateway_router(registry, context);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("axum::serve exited");
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
        client_info.serve(transport).await.expect("sandbox client connect")
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

        let gateway_addr = spawn_gateway(registry, context).await;
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

        let identity_a = Identity {
            org: "org-a".to_string(),
            workspace: "ws".to_string(),
            user: "u".to_string(),
        };
        let client_a_first = clients
            .get_or_build("alpha", &url, &identity_a)
            .await
            .expect("build client for identity_a");

        // Same identity, same server: reused, not rebuilt.
        let client_a_second = clients
            .get_or_build("alpha", &url, &identity_a)
            .await
            .expect("resolve cached client for identity_a");
        assert!(
            Arc::ptr_eq(&client_a_first, &client_a_second),
            "unchanged identity must reuse the cached client, not rebuild it"
        );

        let identity_b = Identity {
            org: "org-b".to_string(),
            workspace: "ws".to_string(),
            user: "u".to_string(),
        };
        let client_b = clients
            .get_or_build("alpha", &url, &identity_b)
            .await
            .expect("build client for identity_b");

        assert!(
            !Arc::ptr_eq(&client_a_first, &client_b),
            "changed identity must rebuild the upstream client, not reuse the old one"
        );
        let guard = clients.entries.lock().await;
        assert_eq!(
            guard.len(),
            1,
            "the stale identity_a entry for 'alpha' must be evicted, not accumulated"
        );
        assert!(guard.contains_key(&("alpha".to_string(), identity_b)));
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

        let gateway_addr = spawn_gateway(registry, context).await;
        // The sandbox tries to spoof its own identity header when talking to
        // the gateway; the gateway must never forward it.
        let spoofed_headers = HashMap::from([(
            HeaderName::from_static("x-org-id"),
            HeaderValue::from_static("attacker"),
        )]);
        let client = connect_sandbox_client(gateway_addr, "echo-server", spoofed_headers).await;

        let echoed = call_echo(&client).await;

        assert_eq!(echoed, "acme-corp", "spoofed client header must not reach upstream");
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

        let gateway_addr = spawn_gateway(registry, context).await;
        let client = connect_sandbox_client(gateway_addr, "does-not-exist", HashMap::new()).await;

        let err = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()))
            .await
            .expect_err("unknown server must surface as an error");

        assert!(err.to_string().to_lowercase().contains("unknown"));
        client.cancel().await.ok();
    }
}
