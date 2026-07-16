#![cfg(feature = "server")]

//! Gemini-native passthrough surface: `POST /{v1beta,v1,google}/models/{model}:{action}`
//! forwards to Vertex and meters usage (tenant/workspace/user headers) from
//! `usageMetadata`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use synapse::gateway::Gateway;
use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore, UsageEntry};
use synapse::pricing::PricingTable;
use synapse::providers::vertex_auth::VertexAuth;
use synapse::providers::Catalog;
use synapse::routing::table::RouteTable;
use synapse::server::router;
use synapse::vertex_native::VertexNativeProvider;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT: &str = "p";
const MODEL: &str = "gemini-2.5-flash";
const GENERATE_PATH: &str =
    "/v1/projects/p/locations/global/publishers/google/models/gemini-2.5-flash:generateContent";
const STREAM_PATH: &str =
    "/v1/projects/p/locations/global/publishers/google/models/gemini-2.5-flash:streamGenerateContent";

fn vertex_auth() -> Arc<VertexAuth> {
    Arc::new(VertexAuth::with_fetcher(|| {
        Box::pin(async { Ok(("test-token".into(), Duration::from_secs(3600))) })
    }))
}

async fn passthrough_gateway(mock_uri: &str) -> (Gateway, Arc<InMemoryLedger>) {
    let routes = RouteTable::from_toml_str(
        r#"[routes."fast"]
           legs = [{ provider = "vertex", model = "gemini-2.5-flash" }]"#,
    )
    .unwrap();
    let catalog = Catalog::build(
        &HashMap::from([("VERTEX_PROJECT_ID".to_string(), PROJECT.to_string())]),
        &routes.referenced_providers(),
        Duration::from_secs(5),
    )
    .unwrap();
    let vertex_native = VertexNativeProvider::new(
        vertex_auth(),
        PROJECT.into(),
        "global".into(),
        Duration::from_secs(5),
        Some(mock_uri.to_string()),
    );
    let store = Arc::new(InMemoryLedger::default());
    let gw = Gateway::builder()
        .routes(routes)
        .catalog(catalog)
        .pricing(PricingTable::default())
        .ledger(LedgerHandle::spawn(
            store.clone() as Arc<dyn LedgerStore>,
            64,
        ))
        .vertex_native(Some(vertex_native))
        .default_tenant("unattributed")
        .build()
        .unwrap();
    (gw, store)
}

async fn ledger_rows(store: &InMemoryLedger) -> Vec<UsageEntry> {
    for _ in 0..100 {
        {
            let rows = store.entries.lock();
            if !rows.is_empty() {
                return rows.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    store.entries.lock().clone()
}

fn gemini_request(path_and_query: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path_and_query)
        .header("content-type", "application/json")
        .header("x-synapse-tenant", "acme")
        .header("x-synapse-workspace", "ws-9")
        .header("x-synapse-user", "user-42")
        .body(Body::from(
            serde_json::json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn buffered_generate_content_forwards_and_meters() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .and(header("authorization", "Bearer test-token"))
        .and(body_string_contains("\"hi\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "hello"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 11, "totalTokenCount": 18}
        })))
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/v1beta/models/{MODEL}:generateContent"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Response is passed through verbatim (Gemini-native shape, not OpenAI).
    assert_eq!(
        json["candidates"][0]["content"]["parts"][0]["text"],
        "hello"
    );

    let rows = ledger_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tenant, "acme");
    assert_eq!(rows[0].workspace.as_deref(), Some("ws-9"));
    assert_eq!(rows[0].user.as_deref(), Some("user-42"));
    assert_eq!(rows[0].model, MODEL);
    assert_eq!(rows[0].route, MODEL);
    assert_eq!(rows[0].lane, "passthrough");
    assert_eq!(rows[0].input_tokens, 7);
    assert_eq!(rows[0].output_tokens, 11);
    assert_eq!(rows[0].status, "ok");
}

#[tokio::test]
async fn streaming_generate_content_forwards_sse_and_meters_final_usage() {
    let mock = MockServer::start().await;
    let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"he\"}]}}]}\n\n\
               data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"llo\"}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":5}}\n\n";
    Mock::given(method("POST"))
        .and(path(STREAM_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/v1beta/models/{MODEL}:streamGenerateContent?alt=sse"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    // SSE bytes are forwarded untouched.
    assert!(text.contains("\"he\""));
    assert!(text.contains("\"llo\""));

    let rows = ledger_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input_tokens, 3);
    assert_eq!(rows[0].output_tokens, 5);
    assert_eq!(rows[0].user.as_deref(), Some("user-42"));
    assert_eq!(rows[0].status, "ok");
}

#[tokio::test]
async fn streaming_survives_beyond_the_buffered_client_timeout() {
    // The harness client timeout is 5s; a stream that takes longer must still
    // complete (streamed passthrough uses its own generous ceiling).
    let mock = MockServer::start().await;
    let sse = "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"slow\"}]}}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":4}}\n\n";
    Mock::given(method("POST"))
        .and(path(STREAM_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(6))
                .set_body_string(sse),
        )
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/v1beta/models/{MODEL}:streamGenerateContent?alt=sse"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("\"slow\""));

    let rows = ledger_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].output_tokens, 4);
    assert_eq!(rows[0].status, "ok");
}

#[tokio::test]
async fn google_api_version_prefix_is_also_routed() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2}
        })))
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/google/models/{MODEL}:generateContent"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(ledger_rows(&store).await.len(), 1);
}

#[tokio::test]
async fn upstream_error_is_forwarded_and_metered_as_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "error": {"code": 429, "message": "quota"}
        })))
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/v1beta/models/{MODEL}:generateContent"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    let rows = ledger_rows(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    assert_eq!(rows[0].input_tokens, 0);
}

#[tokio::test]
async fn count_tokens_is_forwarded_but_not_metered() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/p/locations/global/publishers/google/models/gemini-2.5-flash:countTokens",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"totalTokens": 9})),
        )
        .mount(&mock)
        .await;

    let (gw, store) = passthrough_gateway(&mock.uri()).await;
    let app = router(Arc::new(gw));

    let resp = app
        .oneshot(gemini_request(&format!(
            "/v1beta/models/{MODEL}:countTokens"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(store.entries.lock().is_empty());
}
