//! axum surface: a thin HTTP layer that delegates to the in-process `Gateway`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::error::GatewayError;
use crate::gateway::{Gateway, GuardedStream, RequestCtx};
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
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(AppState { gateway })
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

async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, GatewayError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    // Share one id between the ledger row (via the gateway) and the response body.
    let ctx = RequestCtx {
        request_id: Some(request_id.clone()),
        ..request_ctx(&headers)
    };

    if req.stream == Some(true) {
        let stream = st.gateway.chat_stream(req, &ctx).await?;
        return Ok(Sse::new(sse_body(stream, request_id)).into_response());
    }

    let completion = st.gateway.chat(req, &ctx).await?;
    Ok(Json(openai_json(&completion, &request_id)).into_response())
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
