use std::collections::HashSet;
use std::time::Duration;

use synapse::providers::Catalog;
use synapse::routing::executor::execute_chain;
use synapse::routing::request::ChatRequest;
use synapse::routing::table::ChainLeg;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn chat_body(model: &str) -> ChatRequest {
    serde_json::from_value(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}]
    }))
    .unwrap()
}

fn ok_response(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "x", "object": "chat.completion", "created": 0, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    })
}

async fn catalog_for(base: &str) -> Catalog {
    let env = std::collections::HashMap::from([
        ("DASHSCOPE_API_KEY".to_string(), "sk-test".to_string()),
        ("DASHSCOPE_BASE_URL".to_string(), format!("{base}/v1")),
    ]);
    Catalog::build(&env, &HashSet::from(["qwen".to_string()]), Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn primary_success_returns_completion_with_usage() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response("hello")))
        .mount(&mock)
        .await;

    let catalog = catalog_for(&mock.uri()).await;
    let legs = vec![ChainLeg { provider: "qwen".into(), model: "qwen-max".into() }];
    let c = execute_chain(&catalog, "gemini-pro", &legs, &chat_body("gemini-pro")).await.unwrap();
    assert_eq!(c.content, "hello");
    assert_eq!(c.input_tokens, 3);
    assert_eq!(c.output_tokens, 5);
    assert_eq!(c.provider, "qwen");
}

#[tokio::test]
async fn all_legs_fail_yields_all_legs_failed() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;

    let catalog = catalog_for(&mock.uri()).await;
    let legs = vec![ChainLeg { provider: "qwen".into(), model: "qwen-max".into() }];
    let err = execute_chain(&catalog, "gemini-pro", &legs, &chat_body("gemini-pro")).await.unwrap_err();
    match err {
        synapse::error::GatewayError::AllLegsFailed { failures, .. } => assert_eq!(failures.len(), 1),
        other => panic!("expected AllLegsFailed, got {other:?}"),
    }
}

/// Regression test: a 503 on leg-1 must cause failover to leg-2, not abort the chain.
///
/// Before the `is_genai_retryable` fix, 5xx errors were not classified as retryable
/// because the string check looked for `" 503"` (space prefix) while genai's actual
/// display format uses a single-quote prefix (`'503 ...`). This caused `execute_chain`
/// to `break` on the first leg failure instead of advancing to the next leg.
#[tokio::test]
async fn first_leg_5xx_falls_over_to_second_leg() {
    // leg 1 (qwen / DASHSCOPE) always returns 503
    let mock1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock1)
        .await;

    // leg 2 (openai) succeeds
    let mock2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response("from-leg-2")))
        .mount(&mock2)
        .await;

    let env = std::collections::HashMap::from([
        ("DASHSCOPE_API_KEY".to_string(), "sk-test".to_string()),
        ("DASHSCOPE_BASE_URL".to_string(), format!("{}/v1", mock1.uri())),
        ("OPENAI_API_KEY".to_string(), "sk-test".to_string()),
        ("OPENAI_BASE_URL".to_string(), format!("{}/v1", mock2.uri())),
    ]);
    let catalog = synapse::providers::Catalog::build(
        &env,
        &std::collections::HashSet::from(["qwen".to_string(), "openai".to_string()]),
        std::time::Duration::from_secs(5),
    )
    .unwrap();

    let legs = vec![
        ChainLeg { provider: "qwen".into(), model: "qwen-max".into() },
        ChainLeg { provider: "openai".into(), model: "gpt-x".into() },
    ];
    let c = execute_chain(&catalog, "gemini-pro", &legs, &chat_body("gemini-pro")).await.unwrap();
    assert_eq!(c.content, "from-leg-2");
    assert_eq!(c.provider, "openai");
}

#[tokio::test]
async fn http_unknown_model_returns_404() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore};
    use synapse::pricing::PricingTable;
    use synapse::providers::Catalog;
    use synapse::routing::table::RouteTable;
    use synapse::server::{router, AppState};
    use tower::ServiceExt;

    let routes = RouteTable::from_toml_str(
        r#"[routes."fast"]
           legs = [{ provider = "qwen", model = "qwen-max" }]"#,
    )
    .unwrap();
    let catalog = Catalog::build(
        &std::collections::HashMap::from([("DASHSCOPE_API_KEY".to_string(), "sk".to_string())]),
        &routes.referenced_providers(),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let ledger = LedgerHandle::spawn(Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>, 16);
    let state = AppState {
        routes: Arc::new(routes),
        catalog: Arc::new(catalog),
        pricing: Arc::new(PricingTable::default()),
        ledger,
        default_tenant: "unattributed".into(),
        vertex_native: None,
    };

    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"nope","messages":[{"role":"user","content":"hi"}]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_happy_path_returns_completion_and_records_ledger() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore};
    use synapse::pricing::PricingTable;
    use synapse::providers::Catalog;
    use synapse::routing::table::RouteTable;
    use synapse::server::{router, AppState};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response("hi there")))
        .mount(&mock)
        .await;

    let routes = RouteTable::from_toml_str(
        r#"[routes."fast"]
           legs = [{ provider = "qwen", model = "qwen-max" }]"#,
    )
    .unwrap();
    let catalog = Catalog::build(
        &std::collections::HashMap::from([
            ("DASHSCOPE_API_KEY".to_string(), "sk".to_string()),
            ("DASHSCOPE_BASE_URL".to_string(), format!("{}/v1", mock.uri())),
        ]),
        &routes.referenced_providers(),
        std::time::Duration::from_secs(5),
    )
    .unwrap();

    let store = Arc::new(InMemoryLedger::default());
    let ledger = LedgerHandle::spawn(store.clone() as Arc<dyn LedgerStore>, 64);
    let state = AppState {
        routes: Arc::new(routes),
        catalog: Arc::new(catalog),
        pricing: Arc::new(PricingTable::default()),
        ledger,
        default_tenant: "unattributed".into(),
        vertex_native: None,
    };

    let resp = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-synapse-tenant", "acme")
                .body(Body::from(
                    r#"{"model":"fast","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "hi there");
    assert_eq!(v["usage"]["prompt_tokens"], 3);

    // ledger is fire-and-forget; wait briefly for the background writer to drain
    for _ in 0..50 {
        if store.entries.lock().len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let entries = store.entries.lock();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tenant, "acme");
}

#[tokio::test]
async fn http_native_feature_on_non_vertex_route_returns_400() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore};
    use synapse::pricing::PricingTable;
    use synapse::providers::Catalog;
    use synapse::routing::table::RouteTable;
    use synapse::server::{router, AppState};
    use tower::ServiceExt;

    let routes = RouteTable::from_toml_str(
        r#"[routes."fast"]
           legs = [{ provider = "qwen", model = "qwen-max" }]"#,
    )
    .unwrap();
    let catalog = Catalog::build(
        &std::collections::HashMap::from([("DASHSCOPE_API_KEY".to_string(), "sk".to_string())]),
        &routes.referenced_providers(),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let ledger =
        LedgerHandle::spawn(Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>, 16);
    let state = AppState {
        routes: Arc::new(routes),
        catalog: Arc::new(catalog),
        pricing: Arc::new(PricingTable::default()),
        ledger,
        default_tenant: "unattributed".into(),
        vertex_native: None,
    };

    let resp = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"fast","messages":[{"role":"user","content":"hi"}],"vertex":{"cached_content":"cachedContents/x"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_streaming_returns_sse_chunks() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore};
    use synapse::pricing::PricingTable;
    use synapse::providers::Catalog;
    use synapse::routing::table::RouteTable;
    use synapse::server::{router, AppState};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response("streamed")))
        .mount(&mock).await;

    let routes = RouteTable::from_toml_str(
        r#"[routes."fast"]
           legs = [{ provider = "qwen", model = "qwen-max" }]"#).unwrap();
    let catalog = Catalog::build(
        &std::collections::HashMap::from([
            ("DASHSCOPE_API_KEY".to_string(), "sk".to_string()),
            ("DASHSCOPE_BASE_URL".to_string(), format!("{}/v1", mock.uri())),
        ]),
        &routes.referenced_providers(), std::time::Duration::from_secs(5)).unwrap();
    let state = AppState {
        routes: Arc::new(routes), catalog: Arc::new(catalog), pricing: Arc::new(PricingTable::default()),
        ledger: LedgerHandle::spawn(Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>, 16),
        default_tenant: "unattributed".into(), vertex_native: None,
    };
    let resp = router(state).oneshot(Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"fast","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)).unwrap())
        .await.unwrap();
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/event-stream"), "content-type was: {ct}");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("chat.completion.chunk"), "body: {text}");
    assert!(text.contains("streamed"), "body: {text}");
    assert!(text.contains("[DONE]"), "body: {text}");
}
