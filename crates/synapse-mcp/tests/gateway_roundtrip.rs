//! End-to-end integration test for Task 5's wiring: an in-process rmcp
//! "platform" upstream, registered through `mcp_admin_router` (the same
//! admin surface `synapse-proxy`'s admin listener merges in), driven
//! through `mcp_gateway_router` by a real rmcp client — proving the whole
//! registry → gateway → identity-injection path end to end, not just the
//! individual units.
//!
//! Also proves the fail-closed contract: with no `ContextStore` binding, the
//! call through the gateway is rejected before ever reaching the upstream.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::Router;
use http_body_util::BodyExt;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ErrorData as McpError, RoleClient, ServerHandler, ServiceExt};
use synapse_context::ContextStore;
use synapse_mcp::{
    mcp_admin_router, mcp_gateway_router, IdentityHeaderRule, McpGatewayConfig, McpRegistry,
};
use tower::ServiceExt as _; // oneshot

/// Generic config reproducing the org/workspace/user identity contract for
/// this integration test — this crate itself has no hardcoded notion of
/// identity, so the test supplies the rules the way a downstream broker
/// would.
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

/// Fake "platform" upstream: a single tool `dispatch_echo` that echoes the
/// caller's `x-org-id` header back as the tool result text (empty string if
/// absent).
#[derive(Clone, Default)]
struct PlatformUpstream;

impl ServerHandler for PlatformUpstream {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "dispatch_echo",
            "echoes the caller's x-org-id header back as tool result text",
            serde_json::Map::new(),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        if request.name != "dispatch_echo" {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }
        let org_id = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.headers.get("x-org-id"))
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        Ok(rmcp::model::CallToolResult::success(vec![
            ContentBlock::text(org_id),
        ]))
    }
}

async fn spawn_platform_upstream() -> SocketAddr {
    let service = StreamableHttpService::new(
        || Ok(PlatformUpstream),
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
    registry: std::sync::Arc<McpRegistry>,
    context: std::sync::Arc<ContextStore>,
    config: std::sync::Arc<McpGatewayConfig>,
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

/// Register `name` -> `url` in the registry via the same admin router
/// `synapse-proxy`'s admin listener merges in (not a direct `registry.register`
/// call), proving the admin HTTP surface actually reaches the registry the
/// gateway resolves against.
async fn register_via_admin_router(registry: std::sync::Arc<McpRegistry>, name: &str, url: &str) {
    let admin = mcp_admin_router(registry);
    let body = serde_json::json!({ "name": name, "url": url }).to_string();
    let resp = admin
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/internal/mcp/servers")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
    let _ = resp.into_body().collect().await;
}

async fn connect_client(
    gateway_addr: SocketAddr,
    server_name: &str,
) -> rmcp::service::RunningService<RoleClient, ClientInfo> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!(
        "http://{gateway_addr}/mcp/{server_name}"
    ));
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

#[tokio::test]
async fn bound_identity_reaches_platform_upstream_registered_via_admin_router() {
    let upstream_addr = spawn_platform_upstream().await;

    let registry = std::sync::Arc::new(McpRegistry::new());
    register_via_admin_router(
        registry.clone(),
        "platform",
        &format!("http://{upstream_addr}/mcp"),
    )
    .await;

    let context = std::sync::Arc::new(ContextStore::new(HashMap::new()));
    context.push(
        HashMap::from([
            ("org".to_string(), "acme-corp".to_string()),
            ("workspace".to_string(), "ws-1".to_string()),
            ("user".to_string(), "u-42".to_string()),
        ]),
        None,
    );

    let config = std::sync::Arc::new(three_required_rules());
    let gateway_addr = spawn_gateway(registry, context, config).await;
    let client = connect_client(gateway_addr, "platform").await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("dispatch_echo").with_arguments(serde_json::Map::new()),
        )
        .await
        .expect("dispatch_echo call through gateway must succeed when identity is bound");

    let echoed = result
        .content
        .first()
        .and_then(ContentBlock::as_text)
        .map(|text| text.text.clone())
        .unwrap_or_default();
    assert_eq!(echoed, "acme-corp");

    client.cancel().await.ok();
}

#[tokio::test]
async fn call_is_rejected_when_no_identity_is_bound() {
    let upstream_addr = spawn_platform_upstream().await;

    let registry = std::sync::Arc::new(McpRegistry::new());
    register_via_admin_router(
        registry.clone(),
        "platform",
        &format!("http://{upstream_addr}/mcp"),
    )
    .await;

    // No `context.push(...)` — the store has only an (empty) base, so the
    // gateway must fail closed rather than forward the call upstream.
    let context = std::sync::Arc::new(ContextStore::new(HashMap::new()));

    let config = std::sync::Arc::new(three_required_rules());
    let gateway_addr = spawn_gateway(registry, context, config).await;
    let client = connect_client(gateway_addr, "platform").await;

    let err = client
        .call_tool(
            CallToolRequestParams::new("dispatch_echo").with_arguments(serde_json::Map::new()),
        )
        .await
        .expect_err("unbound identity must be rejected, not forwarded upstream");

    assert!(
        err.to_string().to_lowercase().contains("context not bound")
            || err.to_string().to_lowercase().contains("not bound"),
        "expected a fail-closed 'not bound' error, got: {err}"
    );

    client.cancel().await.ok();
}
