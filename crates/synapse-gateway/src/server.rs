//! axum surface: a thin HTTP layer that delegates to the in-process `Gateway`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tap::Tap;

use crate::error::GatewayError;
use crate::gateway::{Gateway, GuardedStream, RequestCtx};
use crate::harness_planner::UhpCreateRequest;
use crate::routing::harness_jev::HarnessRoutingReport;
use crate::routing::jev_router::RoutingReport;
use crate::routing::request::ChatRequest;
use crate::routing::stream::{stream_item_to_sse_json, Accumulator, StreamItem};

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
}

pub fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(list_models))
        .route("/v1/harnesses", get(list_harnesses))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(create_uhp_response))
        .route("/v1/responses/{id}", get(get_uhp_response))
        .route("/v1/embeddings", post(embeddings))
        // Gemini-native passthrough: `@google/generative-ai` / `@google/genai`
        // clients pointed at the gateway via GOOGLE_VERTEX_BASE_URL. The three
        // prefixes cover the SDK `apiVersion` values in use (default v1beta,
        // Vertex-style `google`, and v1).
        .route("/v1beta/models/{model_action}", post(gemini_passthrough))
        .route("/v1/models/{model_action}", post(gemini_passthrough))
        .route("/google/models/{model_action}", post(gemini_passthrough))
        // TypeSafe System One (Jev) passthrough: forwards `{state, questions}`
        // bodies verbatim — Jev has no OpenAI-shaped equivalent.
        .route("/typesafe/v1/systemone", post(jev_passthrough))
        .with_state(AppState { gateway })
}

/// The API app (gateway plus public A2A routes) and, unless `shared_admin`
/// merges them into it, a separate app for the A2A admin routes.
pub fn apps(
    gateway: Arc<Gateway>,
    registry: Arc<synapse_a2a::A2aRegistry>,
    shared_admin: bool,
) -> (Router, Option<Router>) {
    let api = router(gateway).merge(synapse_a2a::a2a_public_router(registry.clone()));
    let admin = synapse_a2a::a2a_admin_router(registry);
    if shared_admin {
        (api.merge(admin), None)
    } else {
        (api, Some(admin))
    }
}

/// `request_ctx` with its request id resolved once (the caller's, else a
/// fresh UUID), so the guard row and the lane's usage row share it.
fn stamped_ctx(headers: &HeaderMap) -> (RequestCtx, String) {
    let ctx = request_ctx(headers);
    let request_id = ctx
        .resolved_request_id()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    (
        RequestCtx {
            request_id: Some(request_id.clone()),
            ..ctx
        },
        request_id,
    )
}

fn request_ctx(headers: &HeaderMap) -> RequestCtx {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    RequestCtx {
        tenant: header("x-synapse-tenant"),
        workspace: header("x-synapse-workspace"),
        user: header("x-synapse-user"),
        thread: header("x-synapse-thread"),
        message: header("x-synapse-message"),
        user_task_type: header("x-synapse-user-task-type"),
        ai_task_type: header("x-synapse-ai-task-type"),
        request_id: None,
    }
}

async fn list_models(State(st): State<AppState>) -> impl IntoResponse {
    let data = st
        .gateway
        .model_aliases()
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "synapse" }))
        .collect::<Vec<_>>();
    Json(json!({ "object": "list", "data": data }))
}

async fn list_harnesses(State(st): State<AppState>) -> impl IntoResponse {
    let data = st
        .gateway
        .harnesses
        .harnesses()
        .iter()
        .map(|(id, cfg)| {
            json!({
                "id": id,
                "object": "harness",
                "name": cfg.name,
                "base": cfg.base,
                "route": cfg.route,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "object": "list", "data": data }))
}

async fn create_uhp_response(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UhpCreateRequest>,
) -> Result<Response, GatewayError> {
    let (ctx, _request_id) = stamped_ctx(&headers);
    let (body, routing) = st.gateway.uhp_response(req, &ctx).await?;
    Ok(with_harness_routing_headers(
        Json(body).into_response(),
        &routing,
    ))
}

async fn get_uhp_response(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Response, GatewayError> {
    st.gateway
        .get_uhp_response(&id)
        .map(|body| Json(body).into_response())
        .ok_or(GatewayError::UnknownModel(id))
}

fn with_harness_routing_headers(response: Response, routing: &HarnessRoutingReport) -> Response {
    routing
        .headers()
        .into_iter()
        .filter_map(|(name, value)| HeaderValue::from_str(&value).ok().map(|v| (name, v)))
        .fold(response, |r, (name, v)| {
            r.tap_mut(|r| {
                r.headers_mut().insert(name, v);
            })
        })
}

async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, GatewayError> {
    // Prefer an explicit message id for correlation when the client stamps
    // x-synapse-message; otherwise mint a UUID shared by ledger + response body.
    let headers_ctx = request_ctx(&headers);
    let request_id = headers_ctx
        .resolved_request_id()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let ctx = RequestCtx {
        request_id: Some(request_id.clone()),
        ..headers_ctx
    };

    if req.stream == Some(true) {
        let stream = st.gateway.chat_stream(req, &ctx).await?;
        let routing = stream.routing().clone();
        return Ok(with_routing_headers(
            Sse::new(sse_body(stream, request_id)).into_response(),
            &routing,
        ));
    }

    let (outcome, routing) = st.gateway.chat_routed(req, &ctx).await?;
    let response = match outcome {
        crate::gateway::ChatOutcome::Plain(completion) => {
            Json(openai_json(&completion, &request_id)).into_response()
        }
        crate::gateway::ChatOutcome::Hybrid(h) => {
            Json(hybrid_json(&h, &request_id)).into_response()
        }
    };
    Ok(with_routing_headers(response, &routing))
}

/// Values that are not valid header text (e.g. non-ASCII tier names) are
/// skipped rather than failing the response.
fn with_routing_headers(response: Response, routing: &RoutingReport) -> Response {
    routing
        .headers()
        .into_iter()
        .filter_map(|(name, value)| HeaderValue::from_str(&value).ok().map(|v| (name, v)))
        .fold(response, |r, (name, v)| {
            r.tap_mut(|r| {
                r.headers_mut().insert(name, v);
            })
        })
}

async fn embeddings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<crate::embeddings::EmbeddingRequest>,
) -> Result<Response, GatewayError> {
    let ctx = request_ctx(&headers);
    let (resp, decision) = st.gateway.embed_routed(req, ctx).await?;
    Ok(with_guard_header(Json(resp).into_response(), &decision))
}

/// Adds `x-synapse-guard: masked` when the screen rewrote the request.
fn with_guard_header(mut resp: Response, d: &crate::guard::GuardDecision) -> Response {
    if d.masked {
        resp.headers_mut()
            .insert("x-synapse-guard", HeaderValue::from_static("masked"));
    }
    resp
}

/// Gemini-native passthrough (`POST .../models/{model}:{action}`). The
/// gateway's own errors use Google's error shape; upstream responses pass
/// through unchanged.
async fn gemini_passthrough(
    state: State<AppState>,
    model_action: axum::extract::Path<String>,
    query: axum::extract::RawQuery,
    headers: HeaderMap,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return gemini_error(GatewayError::BadRequest(rejection.body_text())),
    };
    forward_gemini(state, model_action, query, headers, body)
        .await
        .unwrap_or_else(gemini_error)
}

fn gemini_error(e: GatewayError) -> Response {
    let status = e.status();
    let (reason, metadata, message) = match &e {
        GatewayError::ContentBlocked {
            policy,
            scanners,
            patterns,
        } => (
            "CONTENT_BLOCKED".to_string(),
            json!({
                "policy": policy,
                "scanners": scanners.join(","),
                "patterns": patterns.join(","),
            }),
            format!(
                "Request blocked by content policy '{policy}' (scanners: {})",
                scanners.join(", ")
            ),
        ),
        GatewayError::UnknownModel(model) => (
            "UNKNOWN_MODEL".to_string(),
            json!({ "model": model }),
            e.to_string(),
        ),
        GatewayError::AllLegsFailed { route, failures } => (
            "ALL_LEGS_FAILED".to_string(),
            json!({
                "route": route,
                "legs_attempted": failures.len().to_string(),
            }),
            e.to_string(),
        ),
        other => (
            other.code().to_ascii_uppercase(),
            json!({}),
            other.to_string(),
        ),
    };
    let body = json!({ "error": {
        "code": status.as_u16(),
        "status": google_status(status),
        "message": message,
        "details": [{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": reason,
            "domain": "synapse-gateway",
            "metadata": metadata,
        }],
    }});
    (status, Json(body)).into_response()
}

fn google_status(status: axum::http::StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "INTERNAL",
    }
}

/// Passthrough actions that send no content upstream to a model and are
/// forwarded unscanned. Every other action is guarded.
const NON_CONTENT_ACTIONS: &[&str] = &["countTokens", "computeTokens"];

/// Passthrough actions whose body is a Gemini request read by
/// `gemini_leaves`. Any other guarded action scans every string in its body.
const GEMINI_BODY_ACTIONS: &[&str] = &[
    "generateContent",
    "streamGenerateContent",
    "embedContent",
    "batchEmbedContents",
];

/// Rejects a path model outside `[A-Za-z0-9._-]+` or an action outside
/// `[A-Za-z]+`, so neither can change the upstream URL's path or query.
fn check_model_action(model: &str, action: &str) -> Result<(), GatewayError> {
    let model_ok = !model.is_empty()
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let action_ok = !action.is_empty() && action.chars().all(|c| c.is_ascii_alphabetic());
    if model_ok && action_ok {
        Ok(())
    } else {
        Err(GatewayError::BadRequest(format!(
            "invalid model or action in models/{model}:{action}"
        )))
    }
}

/// Forwards the request to Vertex via the native provider and meters usage
/// from the response's `usageMetadata`. Generation actions walk consecutive
/// Vertex legs from `routes.toml` on 429/5xx (see
/// `RouteTable::vertex_fallback_chain`); other actions are a single forward.
async fn forward_gemini(
    State(st): State<AppState>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Result<Response, GatewayError> {
    let provider = st
        .gateway
        .vertex_native
        .as_ref()
        .ok_or_else(|| {
            GatewayError::BadRequest("gemini passthrough requires the native vertex lane".into())
        })?
        .clone();

    let (model, action) = model_action.rsplit_once(':').ok_or_else(|| {
        GatewayError::BadRequest(format!(
            "expected models/{{model}}:{{action}}, got '{model_action}'"
        ))
    })?;
    check_model_action(model, action)?;
    let alt_sse = query.as_deref().is_some_and(|q| q.contains("alt=sse"));
    let streaming = action == "streamGenerateContent";
    let metered = action == "generateContent" || streaming;

    let chain = st.gateway.routes.vertex_fallback_chain(model);
    if chain.route.is_none()
        && st.gateway.passthrough_models == crate::config::PassthroughModels::Routed
    {
        return Err(GatewayError::UnknownModel(model.to_string()));
    }
    let Some(first_leg) = chain.legs.first().cloned() else {
        return Err(GatewayError::BadRequest(format!(
            "route '{model}' has no leading vertex legs for the gemini passthrough"
        )));
    };

    let (ctx, request_id) = stamped_ctx(&headers);
    let route_name = chain.route.as_deref().unwrap_or(model);
    let decision = if NON_CONTENT_ACTIONS.contains(&action) {
        crate::guard::GuardDecision::default()
    } else {
        screen_gemini(&st, model, route_name, action, &mut body, &ctx, &request_id).await?
    };

    // Non-generation actions: pure single forward, no chain / metering.
    if !metered {
        let resp = provider
            .passthrough_request(
                &first_leg.model,
                action,
                false,
                body,
                first_leg.region.as_deref(),
            )
            .await?;
        let status = resp.status();
        st.gateway
            .metrics
            .passthrough("vertex", &first_leg.model, action, status.is_success());
        let bytes = resp.bytes().await.map_err(|e| GatewayError::Upstream {
            status: 502,
            body: e.to_string(),
        })?;
        return passthrough_response(status.as_u16(), "application/json", bytes)
            .map(|r| with_guard_header(r, &decision));
    }

    forward_gemini_legs(&st, &ctx, provider, chain, action, streaming, alt_sse, body)
        .await
        .map(|r| with_guard_header(r, &decision))
}

/// Screens a Gemini passthrough body under every policy of the routes that
/// list `model` (see `RouteTable::passthrough_policies`), in place. The
/// request is masked when any policy masked it.
async fn screen_gemini(
    st: &AppState,
    model: &str,
    route: &str,
    action: &str,
    body: &mut serde_json::Value,
    ctx: &RequestCtx,
    request_id: &str,
) -> Result<crate::guard::GuardDecision, GatewayError> {
    let mut decision = crate::guard::GuardDecision::default();
    for policy in st.gateway.routes.passthrough_policies(model) {
        let leaves = if GEMINI_BODY_ACTIONS.contains(&action) {
            crate::guard::leaves::gemini_leaves(body)
        } else {
            crate::guard::leaves::body_leaves(body)
        };
        let d = st
            .gateway
            .guard_lane(
                route,
                Some(policy),
                crate::guard::GuardLane::Passthrough,
                leaves,
                ctx,
                request_id,
            )
            .await?;
        decision.masked |= d.masked;
    }
    Ok(decision)
}

#[allow(clippy::too_many_arguments)]
async fn forward_gemini_legs(
    st: &AppState,
    ctx: &RequestCtx,
    provider: Arc<crate::vertex_native::VertexNativeProvider>,
    chain: crate::routing::table::VertexPassthroughChain,
    action: &str,
    streaming: bool,
    alt_sse: bool,
    body: serde_json::Value,
) -> Result<Response, GatewayError> {
    use axum::body::Body;
    use axum::http::{header, StatusCode};

    let route_alias = chain.route.as_deref();
    let mut prev_model: Option<String> = None;
    let mut last_failure: Option<(u16, axum::body::Bytes)> = None;

    for (i, leg) in chain.legs.iter().enumerate() {
        if let Some(from) = prev_model.take() {
            st.gateway.metrics.passthrough_fallback(&from, &leg.model);
        }

        let attempt = provider
            .passthrough_request(
                &leg.model,
                action,
                alt_sse && streaming,
                body.clone(),
                leg.region.as_deref(),
            )
            .await;

        let resp = match attempt {
            Ok(r) => r,
            Err(e) => {
                meter_passthrough_error(&st.gateway, ctx, &leg.model, route_alias);
                st.gateway
                    .metrics
                    .passthrough("vertex", &leg.model, action, false);
                if i + 1 < chain.legs.len() {
                    let next = &chain.legs[i + 1];
                    tracing::warn!(
                        target: "synapse::routing",
                        lane = "passthrough",
                        route = route_alias.unwrap_or(&leg.model),
                        from_provider = "vertex",
                        from_model = %leg.model,
                        to_provider = "vertex",
                        to_model = %next.model,
                        reason = "error",
                        "leg failed; falling back to next leg"
                    );
                    prev_model = Some(leg.model.clone());
                    continue;
                }
                return Err(e);
            }
        };

        let status = resp.status();
        st.gateway
            .metrics
            .passthrough("vertex", &leg.model, action, status.is_success());

        if status.is_success() {
            let mut guard = PassthroughUsageGuard::new(
                &st.gateway,
                ctx,
                &leg.model,
                route_alias,
                "vertex",
                "chat",
            );
            if streaming && alt_sse {
                let content_type = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/event-stream")
                    .to_string();
                let metered_stream = MeteredSseStream {
                    inner: resp.bytes_stream(),
                    guard,
                    line_buf: String::new(),
                };
                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from_stream(metered_stream))
                    .map_err(|e| GatewayError::Upstream {
                        status: 502,
                        body: e.to_string(),
                    })?;
                return Ok(with_passthrough_leg_headers(response, i + 1, true));
            }

            let bytes = resp.bytes().await.map_err(|e| GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            })?;
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                guard.observe_usage_metadata(&value);
            }
            drop(guard);
            return passthrough_response(status.as_u16(), "application/json", bytes)
                .map(|r| with_passthrough_leg_headers(r, i + 1, true));
        }

        let bytes = resp.bytes().await.unwrap_or_default();
        meter_passthrough_error(&st.gateway, ctx, &leg.model, route_alias);

        if passthrough_status_retryable(status) && i + 1 < chain.legs.len() {
            let next = &chain.legs[i + 1];
            tracing::warn!(
                target: "synapse::routing",
                lane = "passthrough",
                route = route_alias.unwrap_or(&leg.model),
                from_provider = "vertex",
                from_model = %leg.model,
                to_provider = "vertex",
                to_model = %next.model,
                status = status.as_u16(),
                "leg failed; falling back to next leg"
            );
            prev_model = Some(leg.model.clone());
            last_failure = Some((status.as_u16(), bytes));
            continue;
        }
        return passthrough_response(status.as_u16(), "application/json", bytes)
            .map(|r| with_passthrough_leg_headers(r, i + 1, false));
    }

    if let Some((code, bytes)) = last_failure {
        return passthrough_response(code, "application/json", bytes)
            .map(|r| with_passthrough_leg_headers(r, chain.legs.len(), false));
    }
    Err(GatewayError::Upstream {
        status: 502,
        body: "gemini passthrough: empty vertex fallback chain".into(),
    })
}

/// Stamps `x-synapse-legs-attempted` and `x-synapse-routing-degraded` when a
/// multi-leg passthrough chain advanced past the primary leg.
fn with_passthrough_leg_headers(
    mut resp: Response,
    legs_attempted: usize,
    succeeded: bool,
) -> Response {
    if legs_attempted > 1 {
        if let Ok(val) = HeaderValue::from_str(&legs_attempted.to_string()) {
            resp.headers_mut().insert("x-synapse-legs-attempted", val);
        }
        let degraded = if succeeded {
            "leg_escalation"
        } else {
            "all_legs_failed"
        };
        resp.headers_mut().insert(
            "x-synapse-routing-degraded",
            HeaderValue::from_static(degraded),
        );
    }
    resp
}

/// TypeSafe System One (Jev) passthrough (`POST /typesafe/v1/systemone`):
/// forwards the `{state, questions}` body verbatim to TypeSafe's API,
/// re-authenticated with the gateway's own key, and meters from the response's
/// `usage`. Jev answers typed questions with structured decisions and has no
/// chat surface, so — unlike the Gemini passthrough — there is no fallback
/// chain and no SSE variant; upstream statuses and error bodies pass through
/// untranslated.
async fn jev_passthrough(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Result<Response, GatewayError> {
    let provider = st
        .gateway
        .jev_native
        .as_ref()
        .ok_or_else(|| {
            GatewayError::BadRequest(
                "jev passthrough requires TYPESAFE_API_KEY to be configured".into(),
            )
        })?
        .clone();

    if !body.is_object() {
        return Err(GatewayError::BadRequest(
            "expected a JSON object body".into(),
        ));
    }
    if body.get("model").is_none() {
        body["model"] = serde_json::Value::from(crate::jev_native::DEFAULT_MODEL);
    }
    let model = body["model"]
        .as_str()
        .unwrap_or(crate::jev_native::DEFAULT_MODEL)
        .to_string();
    let (ctx, request_id) = stamped_ctx(&headers);
    let decision = st
        .gateway
        .guard_lane(
            &model,
            None,
            crate::guard::GuardLane::Jev,
            crate::guard::leaves::jev_leaves(&mut body),
            &ctx,
            &request_id,
        )
        .await?;
    forward_jev(&st, &ctx, provider, &model, body)
        .await
        .map(|r| with_guard_header(r, &decision))
}

async fn forward_jev(
    st: &AppState,
    ctx: &RequestCtx,
    provider: Arc<crate::jev_native::JevNativeProvider>,
    model: &str,
    body: serde_json::Value,
) -> Result<Response, GatewayError> {
    let mut guard =
        PassthroughUsageGuard::new(&st.gateway, ctx, model, None, "typesafe", "systemone");

    let resp = match provider.evaluate(body).await {
        Ok(r) => r,
        Err(e) => {
            guard.status = "error";
            return Err(e);
        }
    };

    let status = resp.status();
    st.gateway
        .metrics
        .passthrough("typesafe", model, "systemone", status.is_success());

    if status.is_success() {
        let bytes = resp.bytes().await.map_err(|e| {
            guard.status = "error";
            GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            }
        })?;
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            guard.observe_usage_metadata(&value);
        }
        return passthrough_response(status.as_u16(), "application/json", bytes);
    }

    let bytes = resp.bytes().await.unwrap_or_default();
    guard.status = "error";
    passthrough_response(status.as_u16(), "application/json", bytes)
}

fn passthrough_status_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

fn meter_passthrough_error(gateway: &Gateway, ctx: &RequestCtx, model: &str, route: Option<&str>) {
    let mut guard = PassthroughUsageGuard::new(gateway, ctx, model, route, "vertex", "chat");
    guard.status = "error";
    drop(guard);
}

fn passthrough_response(
    status: u16,
    content_type: &str,
    body: axum::body::Bytes,
) -> Result<Response, GatewayError> {
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(body))
        .map_err(|e| GatewayError::Upstream {
            status: 502,
            body: e.to_string(),
        })
}

/// Accumulates response usage counts and fires exactly one ledger row on
/// drop — every termination path (completion, error, client disconnect) meters.
struct PassthroughUsageGuard {
    ledger: crate::ledger::LedgerHandle,
    pricing: std::sync::Arc<crate::pricing::PricingTable>,
    provider: &'static str,
    tenant: String,
    attribution: crate::gateway::Attribution,
    route: String,
    model: String,
    request_id: String,
    input_tokens: u64,
    output_tokens: u64,
    status: &'static str,
    op: &'static str,
}

impl PassthroughUsageGuard {
    fn new(
        gateway: &Gateway,
        ctx: &RequestCtx,
        model: &str,
        route: Option<&str>,
        provider: &'static str,
        op: &'static str,
    ) -> Self {
        Self {
            ledger: gateway.ledger.clone(),
            pricing: gateway.pricing.clone(),
            provider,
            tenant: ctx
                .tenant
                .clone()
                .unwrap_or_else(|| gateway.default_tenant.clone()),
            attribution: gateway.attribution_of(ctx, route.unwrap_or(model)),
            route: route.unwrap_or(model).to_string(),
            model: model.to_string(),
            request_id: ctx
                .resolved_request_id()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            input_tokens: 0,
            output_tokens: 0,
            status: "ok",
            op,
        }
    }

    /// Fold a response's usage into the running totals, overwriting earlier
    /// counts. Both metered shapes are understood: Vertex `usageMetadata`
    /// (counts are cumulative per chunk) and TypeSafe `usage` (single shot).
    fn observe_usage_metadata(&mut self, value: &serde_json::Value) {
        if let Some(n) = value["usage"]["input_tokens"].as_u64() {
            self.input_tokens = n;
        }
        if let Some(n) = value["usage"]["output_tokens"].as_u64() {
            self.output_tokens = n;
        }
        if let Some(n) = value["usageMetadata"]["promptTokenCount"].as_u64() {
            self.input_tokens = n;
        }
        if let Some(n) = value["usageMetadata"]["candidatesTokenCount"].as_u64() {
            self.output_tokens = n;
        }
    }

    fn observe_sse_line(&mut self, line: &str) {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                self.observe_usage_metadata(&value);
            }
        }
    }
}

impl Drop for PassthroughUsageGuard {
    fn drop(&mut self) {
        let cost = self.pricing.cost_usd(
            self.provider,
            &self.model,
            self.input_tokens,
            self.output_tokens,
        );
        self.ledger.enqueue(crate::ledger::UsageEntry {
            ts: chrono::Utc::now(),
            tenant: self.tenant.clone(),
            workspace: self.attribution.workspace.clone(),
            user: self.attribution.user.clone(),
            thread: self.attribution.thread.clone(),
            message: self.attribution.message.clone(),
            route: self.route.clone(),
            provider: self.provider.into(),
            model: self.model.clone(),
            lane: "passthrough".into(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: cost,
            request_id: self.request_id.clone(),
            status: self.status.to_string(),
            op: self.op.into(),
            user_task_type: self.attribution.user_task_type.clone(),
            ai_task_type: self.attribution.ai_task_type.clone(),
            score: None,
            confidence: None,
            tier: None,
            outcome: None,
        });
    }
}

/// Tee a Vertex SSE byte stream to the client while scanning complete
/// `data:` lines for `usageMetadata` (metered by the owned guard on drop).
struct MeteredSseStream<S> {
    inner: S,
    guard: PassthroughUsageGuard,
    line_buf: String,
}

impl<S> futures::Stream for MeteredSseStream<S>
where
    S: futures::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<axum::body::Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        let this = self.get_mut();
        match this.inner.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                this.line_buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = this.line_buf.find('\n') {
                    let line: String = this.line_buf.drain(..=pos).collect();
                    this.guard.observe_sse_line(line.trim_end());
                }
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                this.guard.status = "error";
                std::task::Poll::Ready(Some(Err(std::io::Error::other(e.to_string()))))
            }
            std::task::Poll::Ready(None) => {
                // Flush a final unterminated data line before the guard drops.
                let rest = std::mem::take(&mut this.line_buf);
                this.guard.observe_sse_line(rest.trim_end());
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Build the OpenAI `chat.completion` JSON from a buffered `Completion`
/// (content OR tool_calls) via an `Accumulator`.
fn openai_json(c: &crate::routing::executor::Completion, request_id: &str) -> serde_json::Value {
    let mut acc = Accumulator::default();
    if c.tool_calls.is_empty() {
        acc.push(StreamItem::Delta(c.content.clone()));
    } else {
        for (i, tc) in c.tool_calls.iter().enumerate() {
            acc.push(StreamItem::ToolCallDelta {
                index: i as u32,
                id: Some(tc.id.clone()),
                name: Some(tc.name.clone()),
                args_fragment: tc.arguments.clone(),
            });
        }
    }
    acc.push(StreamItem::Done {
        input_tokens: c.input_tokens,
        output_tokens: c.output_tokens,
        finish_reason: c.finish_reason,
    });
    acc.to_openai_response(request_id, &c.model)
}

/// Render a Jev hybrid-extraction outcome as the stable envelope from the
/// spec: a `jev` block beside the OpenAI-shaped choices/usage, `content` a
/// JSON map of candidate key → extraction result (absent when no survivor
/// was attempted).
fn hybrid_json(h: &crate::gateway::HybridOutcome, request_id: &str) -> serde_json::Value {
    let content = h
        .extraction_ran
        .then(|| serde_json::to_string(&h.extractions).unwrap());
    let mut message = json!({ "role": "assistant" });
    if let Some(content) = content {
        message["content"] = json!(content);
    }
    json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": h.model,
        "jev": {
            "answers": h.answers,
            "survivors": h.survivors,
            "degraded": h.degraded,
        },
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": h.input_tokens,
            "completion_tokens": h.output_tokens,
            "total_tokens": h.input_tokens + h.output_tokens,
        },
    })
}

/// Render a `GuardedStream` as OpenAI SSE (`chat.completion.chunk` … `[DONE]`).
fn sse_body(
    stream: GuardedStream,
    request_id: String,
) -> impl futures::Stream<Item = Result<Event, std::convert::Infallible>> {
    use futures::StreamExt;
    let model = stream.model().to_string();
    stream
        .map(move |item| match item {
            Ok(it) => {
                let json = stream_item_to_sse_json(&it, &request_id, &model);
                Ok(Event::default().data(json.to_string()))
            }
            Err(e) => {
                let err = json!({
                    "error": { "type": "upstream_error", "message": e.to_string(), "code": "upstream_error" }
                });
                Ok(Event::default().data(err.to_string()))
            }
        })
        .chain(futures::stream::once(async { Ok(Event::default().data("[DONE]")) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn gemini_block_metadata_names_the_patterns() {
        let resp = gemini_error(GatewayError::ContentBlocked {
            policy: "strict".into(),
            scanners: vec!["secrets".into()],
            patterns: vec!["aws_access_key".into(), "github_token".into()],
        });
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["error"]["details"][0]["metadata"]["patterns"],
            "aws_access_key,github_token"
        );
    }
}
