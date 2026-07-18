//! Spike: prove the rmcp `StreamableHttpService` round-trip end to end, and
//! settle the single most load-bearing unknown for the gateway — whether
//! `StreamableHttpClientTransport` lets identity headers (like `x-org-id`)
//! vary **per call** or only **per connection** (fixed at transport
//! construction).
//!
//! The in-process server exposes one tool, `echo`, that reads the `x-org-id`
//! header off the raw HTTP request (via `RequestContext::extensions`) and
//! echoes it back as the tool result text. A client is built with that
//! header baked into its `StreamableHttpClientTransportConfig`.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::Router;
use http::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResult, ClientCapabilities,
    ClientInfo, ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};

/// Single-tool MCP server: `echo` returns the caller's `x-org-id` header
/// value (empty string if absent).
#[derive(Clone, Default)]
struct EchoServer;

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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

/// Start the echo server on a loopback ephemeral port; returns the bound address.
async fn spawn_echo_server() -> SocketAddr {
    let service = StreamableHttpService::new(
        || Ok(EchoServer),
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

/// Build a client transport config with `x-org-id: {org_id}` baked in at
/// construction time (the only place `StreamableHttpClientTransportConfig`
/// exposes for custom headers).
fn config_with_org_header(addr: SocketAddr, org_id: &str) -> StreamableHttpClientTransportConfig {
    let mut headers = HashMap::new();
    headers.insert(
        HeaderName::from_static("x-org-id"),
        HeaderValue::from_str(org_id).expect("valid header value"),
    );
    StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
        .custom_headers(headers)
}

/// Connect a client from `config`, call `echo` once, and return the echoed
/// text. Leaves the client connected so callers can issue further calls on
/// the same connection.
async fn call_echo(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>,
) -> String {
    let result = client
        .call_tool(CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()))
        .await
        .expect("call_tool");
    result
        .content
        .first()
        .and_then(ContentBlock::as_text)
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

async fn connect(
    config: StreamableHttpClientTransportConfig,
) -> rmcp::service::RunningService<rmcp::RoleClient, ClientInfo> {
    let transport = StreamableHttpClientTransport::from_config(config);
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("synapse-mcp-spike", "0.0.1"),
    );
    client_info.serve(transport).await.expect("client connect")
}

#[tokio::test]
async fn round_trip_echoes_the_configured_org_header() {
    let addr = spawn_echo_server().await;
    let client = connect(config_with_org_header(addr, "acme-corp")).await;

    let echoed = call_echo(&client).await;

    assert_eq!(echoed, "acme-corp");
    client.cancel().await.expect("cancel");
}

/// THE key finding: headers are fixed at transport construction, not settable
/// per call. Prove it two ways on ONE client connection:
///   1. Calling `echo` twice on the same client yields the same header value
///      both times (nothing in `call_tool`/`CallToolRequestParams` can change
///      it per call — the type has no header field).
///   2. A second, independently-constructed client with a *different* baked-in
///      header produces a *different* echoed value — i.e. the only way to
///      change the identity header is to build a new client/transport, not to
///      vary an existing one.
#[tokio::test]
async fn headers_are_fixed_per_connection_not_per_call() {
    let addr = spawn_echo_server().await;

    let client_a = connect(config_with_org_header(addr, "org-a")).await;
    let first_call = call_echo(&client_a).await;
    let second_call = call_echo(&client_a).await;
    assert_eq!(first_call, "org-a");
    assert_eq!(
        second_call, "org-a",
        "same client/transport must keep echoing the header it was constructed with"
    );
    client_a.cancel().await.expect("cancel client_a");

    // Changing the identity requires a brand new client built from a new
    // config — there is no per-call override on CallToolRequestParams.
    let client_b = connect(config_with_org_header(addr, "org-b")).await;
    let third_call = call_echo(&client_b).await;
    assert_eq!(
        third_call, "org-b",
        "a differently-configured client must echo its own header"
    );
    client_b.cancel().await.expect("cancel client_b");
}
