#![cfg(feature = "server")]

//! End-to-end UHP 2D (`harness × model`) Jev router integration tests:
//! - Turn 1 2D `(profile, tier)` selection + `needs_reasoning` effort bump + `needs_deep_horizon` step bump
//! - Turn 2 continuation (`previous_response_id`) pinning `base` while re-scoring `tier` and omitting `profile` from Jev questions
//! - `409 harness_mismatch` when `previous_response_id` belongs to a different `harness_id`
//! - Independent low profile confidence (`x-synapse-routing-degraded: low_profile_confidence`)
//! - Runner failover across bases in the profile when the primary base returns an error
//! - Ledger `route_decision` + `harness_turn` rows and Prometheus `synapse_harness_*` metrics

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use synapse::error::GatewayError;
use synapse::gateway::Gateway;
use synapse::guard::GuardEngine;
use synapse::harness_planner::{UhpRunner, UhpTurnDispatch, UhpTurnOutput};
use synapse::jev_native::JevNativeProvider;
use synapse::ledger::{InMemoryLedger, LedgerHandle, LedgerStore, UsageEntry};
use synapse::pricing::PricingTable;
use synapse::providers::Catalog;
use synapse::routing::effort::Effort;
use synapse::routing::harness_table::HarnessRouteTable;
use synapse::routing::table::RouteTable;
use synapse::server::router;
use synapse::telemetry::{scrape, test_metrics};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ROUTES: &str = r#"
[routes."fast"]
legs = [{ provider = "qwen", model = "qwen-flash" }]

[routes."balanced"]
legs = [{ provider = "qwen", model = "qwen-plus" }]

[routes."deep"]
legs = [{ provider = "qwen", model = "qwen-max" }]
"#;

const HARNESSES: &str = r#"
[bases.pi]
label = "Pi Coding Agent"
wire = ["openai_chat", "gemini_native"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "hard"

[bases.claude-code]
label = "Claude Code"
wire = ["openai_chat", "anthropic_messages", "vertex_anthropic"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "hard"

[bases.codex]
label = "Codex CLI"
wire = ["openai_chat", "openai_responses"]
skills = true
environments = true
mcp_transports = ["stdio", "http"]
tool_enforcement = "hard"

[bases.hermes]
label = "Hermes Agent"
wire = ["openai_chat"]
skills = true
environments = true
mcp_transports = ["stdio", "http", "sse"]
tool_enforcement = "instruction"

[harnesses."chrn_default"]
name = "default-agent"
base = "auto"
route = "auto"
system_prompt = "You are a helpful software engineering agent."

[harnesses."chrn_strict"]
name = "strict-agent"
base = "pi"
route = "auto"
system_prompt = "Restricted agent."
disabled_tools = ["Bash"]

[harness_routes."auto"]
strategy = "jev"

[harness_routes."auto".jev_router]
model = "jev-latest"
default_profile = "coding"
default_tier = "balanced"
timeout_ms = 2000
min_confidence = 0.5
min_profile_confidence = 0.5
reasoning_threshold = 0.7
horizon_threshold = 0.7

[harness_routes."auto".profiles.coding]
description = "Software engineering, debugging, multi-file edits, and test execution"
bases = ["pi", "claude-code", "codex"]

[harness_routes."auto".profiles.research]
description = "Open-ended research, synthesis, and multi-source investigation"
bases = ["hermes", "pi"]

[[harness_routes."auto".tiers]]
name = "fast"
description = "Quick file inspection, single-command checks, or simple questions"
effort = "low"
max_step = 10
timeout_seconds = 120
legs = [{ route = "fast" }]

[[harness_routes."auto".tiers]]
name = "balanced"
description = "Standard feature implementation, bug fixes, and multi-file edits"
effort = "medium"
max_step = 25
timeout_seconds = 300
legs = [{ route = "balanced" }]

[[harness_routes."auto".tiers]]
name = "deep"
description = "Complex architecture, subtle concurrency bugs, or large refactors"
effort = "high"
max_step = 50
timeout_seconds = 600
legs = [{ route = "deep" }]
"#;

#[derive(Default)]
struct RecordingRunner {
    dispatches: Mutex<Vec<UhpTurnDispatch>>,
    fail_bases: Mutex<Vec<String>>,
}

impl RecordingRunner {
    fn fail_base(self: &Arc<Self>, base: &str) {
        self.fail_bases.lock().unwrap().push(base.to_string());
    }

    fn dispatches(&self) -> Vec<UhpTurnDispatch> {
        self.dispatches.lock().unwrap().clone()
    }
}

#[async_trait]
impl UhpRunner for RecordingRunner {
    async fn execute_turn(
        &self,
        dispatch: &UhpTurnDispatch,
    ) -> Result<UhpTurnOutput, GatewayError> {
        self.dispatches.lock().unwrap().push(dispatch.clone());
        if self
            .fail_bases
            .lock()
            .unwrap()
            .iter()
            .any(|b| b == &dispatch.base)
        {
            return Err(GatewayError::HarnessUnavailable(format!(
                "runner base '{}' is down",
                dispatch.base
            )));
        }
        Ok(UhpTurnOutput {
            output_text: format!(
                "served by {}:{} ({:?})",
                dispatch.base, dispatch.leg.model, dispatch.leg.effort
            ),
            input_tokens: 120,
            output_tokens: 45,
            turns: 2,
        })
    }
}

fn jev_2d_response(
    profile: Option<(&str, f64)>,
    difficulty: (f64, f64),
    needs_reasoning: f64,
    needs_deep_horizon: f64,
) -> Value {
    let mut answers = json!({
        "difficulty": {
            "type": "score",
            "score": difficulty.0,
            "confidence": difficulty.1,
        },
        "needs_reasoning": {
            "type": "noul",
            "noul": needs_reasoning,
        },
        "needs_deep_horizon": {
            "type": "noul",
            "noul": needs_deep_horizon,
        },
    });
    if let Some((choice, conf)) = profile {
        answers["profile"] = json!({
            "type": "choice",
            "choice": choice,
            "confidence": conf,
        });
    }
    json!({
        "model": "jev-1.13-20260917",
        "answers": answers,
        "usage": { "input_tokens": 300, "output_tokens": 25 },
        "provider": "TypeSafe",
    })
}

fn build_test_gateway(
    jev_uri: Option<String>,
) -> (
    Arc<Gateway>,
    Arc<RecordingRunner>,
    Arc<InMemoryLedger>,
    impl Fn() -> String,
) {
    let routes = RouteTable::from_toml_str(ROUTES).unwrap();
    let harnesses = HarnessRouteTable::from_toml_str(HARNESSES, &routes).unwrap();
    let env = HashMap::from([
        ("DASHSCOPE_API_KEY".to_string(), "sk-test".to_string()),
        ("TYPESAFE_API_KEY".to_string(), "sk-test".to_string()),
    ]);
    let catalog =
        Catalog::build(&env, &routes.referenced_providers(), Duration::from_secs(5)).unwrap();
    let store = Arc::new(InMemoryLedger::default());
    let (metrics, exporter) = test_metrics();
    let runner = Arc::new(RecordingRunner::default());
    let gateway = Arc::new(
        Gateway::builder()
            .routes(routes)
            .harnesses(harnesses)
            .uhp_runner(Some(runner.clone()))
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(LedgerHandle::spawn(
                store.clone() as Arc<dyn LedgerStore>,
                64,
            ))
            .metrics(metrics)
            .guard(GuardEngine::empty())
            .jev_native(jev_uri.map(|uri| {
                JevNativeProvider::new("sk-test".into(), Some(uri), Duration::from_secs(5))
            }))
            .build()
            .unwrap(),
    );
    (gateway, runner, store, move || scrape(&exporter))
}

#[tokio::test]
async fn turn1_selects_2d_cell_and_turn2_pins_base_while_rescoring_tier() {
    let jev = MockServer::start().await;
    // Turn 1: Jev selects profile = "research" (first base: "hermes"), difficulty = 0.6 -> "balanced",
    // needs_reasoning = 0.85 -> effort bumped to High, needs_deep_horizon = 0.80 -> max_step bumped to 50.
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jev_2d_response(
            Some(("research", 0.91)),
            (0.60, 0.88),
            0.85,
            0.80,
        )))
        .up_to_n_times(1)
        .mount(&jev)
        .await;

    // Turn 2: continuation omits `profile` question; Jev down-shifts difficulty = 0.10 -> "fast".
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jev_2d_response(
            None,
            (0.10, 0.92),
            0.10,
            0.10,
        )))
        .up_to_n_times(1)
        .mount(&jev)
        .await;

    let (gateway, runner, ledger, metrics_scrape) = build_test_gateway(Some(jev.uri()));
    let app = router(gateway.clone());

    // Turn 1 request
    let req1 = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "auto",
                "input": "Synthesize the architecture across all crates and compare trade-offs.",
                "metadata": { "harness_id": "chrn_default" }
            })
            .to_string(),
        ))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let headers1 = resp1.headers().clone();
    assert_eq!(headers1["x-synapse-routing"], "jev");
    assert_eq!(headers1["x-synapse-profile"], "research");
    assert_eq!(headers1["x-synapse-harness-base"], "hermes");
    assert_eq!(headers1["x-synapse-tier"], "balanced");
    assert_eq!(headers1["x-synapse-reasoning-effort"], "high");
    assert_eq!(headers1["x-synapse-max-step"], "50");

    let bytes1 = resp1.into_body().collect().await.unwrap().to_bytes();
    let body1: Value = serde_json::from_slice(&bytes1).unwrap();
    let resp1_id = body1["id"].as_str().unwrap().to_string();
    let session_id = body1["metadata"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(body1["metadata"]["harness_base"], "hermes");
    assert_eq!(body1["metadata"]["model_tier"], "balanced");

    // Turn 2 continuation
    let req2 = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "auto",
                "previous_response_id": resp1_id,
                "input": "Thanks! Format that summary as a 3-bullet list."
            })
            .to_string(),
        ))
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let headers2 = resp2.headers().clone();
    // Base stays pinned to "hermes", while tier down-shifts to "fast"!
    assert_eq!(headers2["x-synapse-harness-base"], "hermes");
    assert_eq!(headers2["x-synapse-tier"], "fast");
    assert_eq!(headers2["x-synapse-reasoning-effort"], "low");
    assert_eq!(headers2["x-synapse-max-step"], "10");

    let received = jev.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    let turn1_jev: Value = serde_json::from_slice(&received[0].body).unwrap();
    let turn2_jev: Value = serde_json::from_slice(&received[1].body).unwrap();
    assert!(turn1_jev["questions"].get("profile").is_some());
    assert!(
        turn2_jev["questions"].get("profile").is_none(),
        "continuation turn must not ask Jev to re-select profile"
    );
    assert_eq!(turn2_jev["state"]["is_continuation"], true);

    let dispatches = runner.dispatches();
    assert_eq!(dispatches.len(), 2);
    assert_eq!(dispatches[0].base, "hermes");
    assert_eq!(dispatches[0].leg.model, "qwen-plus");
    assert_eq!(dispatches[0].leg.effort, Some(Effort::High));
    assert_eq!(dispatches[1].base, "hermes");
    assert_eq!(dispatches[1].leg.model, "qwen-flash");
    assert_eq!(dispatches[1].leg.effort, Some(Effort::Low));

    let entries = wait_rows(&ledger, 4).await;
    assert!(entries.iter().any(|e| {
        e.op == "route_decision"
            && e.thread.as_deref() == Some(session_id.as_str())
            && e.tier.as_deref() == Some("research/hermes/balanced")
    }));
    assert!(entries.iter().any(|e| {
        e.op == "harness_turn"
            && e.lane == "uhp"
            && e.thread.as_deref() == Some(session_id.as_str())
    }));

    let prom = metrics_scrape();
    assert!(prom.contains("synapse_harness_routing_decisions_total"));
    assert!(prom.contains("synapse_harness_turns_total"));
}

async fn wait_rows(store: &InMemoryLedger, n: usize) -> Vec<UsageEntry> {
    for _ in 0..200 {
        {
            let rows = store.entries.lock();
            if rows.len() >= n {
                return rows.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    store.entries.lock().clone()
}

#[tokio::test]
async fn continuation_with_mismatched_harness_id_returns_409_conflict() {
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jev_2d_response(
            Some(("coding", 0.90)),
            (0.50, 0.90),
            0.20,
            0.20,
        )))
        .mount(&jev)
        .await;

    let (gateway, _runner, _ledger, _scrape) = build_test_gateway(Some(jev.uri()));
    let app = router(gateway);

    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "input": "Initial turn",
                        "metadata": { "harness_id": "chrn_default" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes1 = resp1.into_body().collect().await.unwrap().to_bytes();
    let body1: Value = serde_json::from_slice(&bytes1).unwrap();
    let resp1_id = body1["id"].as_str().unwrap();

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "previous_response_id": resp1_id,
                        "input": "Try switching harness mid-session",
                        "metadata": { "harness_id": "chrn_strict" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::CONFLICT);
    let bytes2 = resp2.into_body().collect().await.unwrap().to_bytes();
    let body2: Value = serde_json::from_slice(&bytes2).unwrap();
    assert_eq!(body2["error"]["code"], "harness_mismatch");
}

#[tokio::test]
async fn low_profile_confidence_falls_back_to_default_profile_and_keeps_decided_tier() {
    let jev = MockServer::start().await;
    // Profile confidence 0.20 < 0.50 -> falls back to default_profile ("coding" -> "pi"),
    // while difficulty confidence 0.91 >= 0.50 -> keeps tier "deep" (score 1.95 -> index 2).
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jev_2d_response(
            Some(("research", 0.20)),
            (1.95, 0.91),
            0.20,
            0.20,
        )))
        .mount(&jev)
        .await;

    let (gateway, _runner, _ledger, _scrape) = build_test_gateway(Some(jev.uri()));
    let app = router(gateway);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "input": "Refactor the distributed lock manager.",
                        "metadata": { "harness_id": "chrn_default" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers();
    assert_eq!(headers["x-synapse-profile"], "coding");
    assert_eq!(headers["x-synapse-harness-base"], "pi");
    assert_eq!(headers["x-synapse-tier"], "deep");
    assert_eq!(
        headers["x-synapse-routing-degraded"],
        "low_profile_confidence"
    );
}

#[tokio::test]
async fn runner_failover_advances_to_secondary_base_in_profile() {
    let jev = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jev_2d_response(
            Some(("coding", 0.90)),
            (0.55, 0.90),
            0.20,
            0.20,
        )))
        .mount(&jev)
        .await;

    let (gateway, runner, _ledger, _scrape) = build_test_gateway(Some(jev.uri()));
    // Primary base in "coding" is "pi"; fail it so the planner advances to "claude-code".
    runner.fail_base("pi");
    let app = router(gateway);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "input": "Fix the failing unit test.",
                        "metadata": { "harness_id": "chrn_default" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-synapse-harness-base"], "claude-code");
    assert_eq!(resp.headers()["x-synapse-tier"], "balanced");
}
