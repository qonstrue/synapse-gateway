//! axum surface: AppState, tenant context, /v1/models, /v1/chat/completions.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use std::convert::Infallible;
use chrono::Utc;
use serde_json::json;
use tap::Pipe;
use uuid::Uuid;

use crate::error::GatewayError;
use crate::ledger::{LedgerHandle, UsageEntry};
use crate::observability::GenAiSpan;
use crate::pricing::PricingTable;
use crate::providers::Catalog;
use crate::routing::classify::{classify, Lane};
use crate::routing::executor::{execute_chain, Completion};
use crate::routing::request::ChatRequest;
use crate::routing::table::RouteTable;
use crate::vertex_native::VertexNativeProvider;

#[derive(Clone)]
pub struct AppState {
    pub routes: Arc<RouteTable>,
    pub catalog: Arc<Catalog>,
    pub pricing: Arc<PricingTable>,
    pub ledger: LedgerHandle,
    pub default_tenant: String,
    pub vertex_native: Option<Arc<VertexNativeProvider>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

struct TenantCtx {
    tenant: String,
    workspace: Option<String>,
}

fn tenant_ctx(headers: &HeaderMap, default_tenant: &str) -> TenantCtx {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    TenantCtx {
        tenant: header("x-synapse-tenant").unwrap_or_else(|| default_tenant.to_string()),
        workspace: header("x-synapse-workspace"),
    }
}

async fn list_models(State(st): State<AppState>) -> impl IntoResponse {
    let data = st
        .routes
        .aliases()
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "synapse" }))
        .collect::<Vec<_>>();
    Json(json!({ "object": "list", "data": data }))
}

async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, GatewayError> {
    let started = Instant::now();
    let ctx = tenant_ctx(&headers, &st.default_tenant);
    let lane = classify(&req);
    let legs = st
        .routes
        .legs(&req.model)
        .ok_or_else(|| GatewayError::UnknownModel(req.model.clone()))?
        .to_vec();
    let request_id = Uuid::new_v4().to_string();

    let completion: Completion = match lane {
        Lane::Standard => execute_chain(&st.catalog, &req.model, &legs, &req).await?,
        Lane::NativeVertex => {
            let leg = legs
                .iter()
                .find(|l| l.provider == "vertex")
                .ok_or_else(|| GatewayError::NativeFeatureUnsupported {
                    feature: "native-vertex".into(),
                    route: req.model.clone(),
                })?;
            st.vertex_native
                .as_ref()
                .ok_or_else(|| GatewayError::BadRequest("native vertex lane not configured".into()))?
                .generate(&leg.model, &req)
                .await?
        }
    };

    // Cost + ledger (fire-and-forget).
    let cost = st.pricing.cost_usd(&completion.provider, &completion.model, completion.input_tokens, completion.output_tokens);
    UsageEntry {
        ts: Utc::now(),
        tenant: ctx.tenant.clone(),
        workspace: ctx.workspace.clone(),
        route: req.model.clone(),
        provider: completion.provider.clone(),
        model: completion.model.clone(),
        lane: match lane { Lane::Standard => "standard".into(), Lane::NativeVertex => "native".into() },
        input_tokens: completion.input_tokens,
        output_tokens: completion.output_tokens,
        cost_usd: cost,
        request_id: request_id.clone(),
        status: "ok".into(),
    }
    .pipe(|entry| st.ledger.enqueue(entry));

    // Observability metrics.
    GenAiSpan::from_completion(&completion, lane, &req.model, &ctx.tenant, ctx.workspace.as_deref(), legs.len() as u32, false)
        .emit_metrics(started.elapsed().as_secs_f64());

    if req.stream == Some(true) {
        return Ok(Sse::new(sse_from_completion(&request_id, &completion)).into_response());
    }
    Ok(Json(openai_response(&request_id, &completion)).into_response())
}

fn sse_from_completion(id: &str, c: &Completion) -> impl Stream<Item = Result<Event, Infallible>> {
    let first = json!({
        "id": format!("chatcmpl-{id}"), "object": "chat.completion.chunk", "created": 0, "model": c.model,
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": c.content }, "finish_reason": null }]
    });
    let done = json!({
        "id": format!("chatcmpl-{id}"), "object": "chat.completion.chunk", "created": 0, "model": c.model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });
    stream::iter(vec![
        Ok(Event::default().data(first.to_string())),
        Ok(Event::default().data(done.to_string())),
        Ok(Event::default().data("[DONE]")),
    ])
}

fn openai_response(id: &str, c: &Completion) -> serde_json::Value {
    json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": c.model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": c.content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": c.input_tokens,
            "completion_tokens": c.output_tokens,
            "total_tokens": c.input_tokens + c.output_tokens
        }
    })
}
