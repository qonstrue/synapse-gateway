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
        .route("/v1/embeddings", post(embeddings))
        // Gemini-native passthrough: `@google/generative-ai` / `@google/genai`
        // clients pointed at the gateway via GOOGLE_VERTEX_BASE_URL. The three
        // prefixes cover the SDK `apiVersion` values in use (default v1beta,
        // Vertex-style `google`, and v1).
        .route("/v1beta/models/{model_action}", post(gemini_passthrough))
        .route("/v1/models/{model_action}", post(gemini_passthrough))
        .route("/google/models/{model_action}", post(gemini_passthrough))
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
        user: header("x-synapse-user"),
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

async fn embeddings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<crate::embeddings::EmbeddingRequest>,
) -> Result<Response, GatewayError> {
    let ctx = request_ctx(&headers);
    let resp = st.gateway.embed(req, ctx).await?;
    Ok(Json(resp).into_response())
}

/// Gemini-native passthrough (`POST .../models/{model}:{action}`): forward the
/// request to Vertex via the native provider and meter usage from the
/// response's `usageMetadata`. Lets Gemini SDK clients (`GOOGLE_VERTEX_BASE_URL`)
/// route through the gateway without translating to the OpenAI surface.
async fn gemini_passthrough(
    State(st): State<AppState>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, GatewayError> {
    use axum::body::Body;
    use axum::http::{header, StatusCode};

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
    let alt_sse = query.as_deref().is_some_and(|q| q.contains("alt=sse"));
    let streaming = action == "streamGenerateContent";

    let resp = provider
        .passthrough_request(model, action, alt_sse && streaming, body)
        .await?;
    let status = resp.status();
    metrics::counter!(
        "synapse_passthrough_total",
        "model" => model.to_string(),
        "action" => action.to_string(),
        "status" => if status.is_success() { "ok" } else { "error" },
    )
    .increment(1);

    // Only generation calls are metered; countTokens etc. are pure forwards.
    let metered = action == "generateContent" || streaming;
    if !metered {
        let bytes = resp.bytes().await.map_err(|e| GatewayError::Upstream {
            status: 502,
            body: e.to_string(),
        })?;
        return passthrough_response(status.as_u16(), "application/json", bytes);
    }

    let ctx = request_ctx(&headers);
    let mut guard = PassthroughUsageGuard::new(&st.gateway, &ctx, model);

    if !status.is_success() {
        let bytes = resp.bytes().await.unwrap_or_default();
        guard.status = "error";
        drop(guard); // meter the failed call
        return passthrough_response(status.as_u16(), "application/json", bytes);
    }

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
        return Ok(response);
    }

    let bytes = resp.bytes().await.map_err(|e| GatewayError::Upstream {
        status: 502,
        body: e.to_string(),
    })?;
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        guard.observe_usage_metadata(&value);
    }
    drop(guard);
    passthrough_response(status.as_u16(), "application/json", bytes)
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

/// Accumulates `usageMetadata` token counts and fires exactly one ledger row on
/// drop — every termination path (completion, error, client disconnect) meters.
struct PassthroughUsageGuard {
    ledger: crate::ledger::LedgerHandle,
    pricing: std::sync::Arc<crate::pricing::PricingTable>,
    tenant: String,
    workspace: Option<String>,
    user: Option<String>,
    model: String,
    request_id: String,
    input_tokens: u64,
    output_tokens: u64,
    status: &'static str,
}

impl PassthroughUsageGuard {
    fn new(gateway: &Gateway, ctx: &RequestCtx, model: &str) -> Self {
        Self {
            ledger: gateway.ledger.clone(),
            pricing: gateway.pricing.clone(),
            tenant: ctx
                .tenant
                .clone()
                .unwrap_or_else(|| gateway.default_tenant.clone()),
            workspace: ctx.workspace.clone(),
            user: ctx.user.clone(),
            model: model.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            input_tokens: 0,
            output_tokens: 0,
            status: "ok",
        }
    }

    /// Fold a Gemini response chunk's `usageMetadata` into the running totals.
    /// Counts are cumulative per Vertex semantics, so later chunks overwrite.
    fn observe_usage_metadata(&mut self, value: &serde_json::Value) {
        let usage = &value["usageMetadata"];
        if let Some(n) = usage["promptTokenCount"].as_u64() {
            self.input_tokens = n;
        }
        if let Some(n) = usage["candidatesTokenCount"].as_u64() {
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
        let cost =
            self.pricing
                .cost_usd("vertex", &self.model, self.input_tokens, self.output_tokens);
        self.ledger.enqueue(crate::ledger::UsageEntry {
            ts: chrono::Utc::now(),
            tenant: self.tenant.clone(),
            workspace: self.workspace.clone(),
            user: self.user.clone(),
            route: self.model.clone(),
            provider: "vertex".into(),
            model: self.model.clone(),
            lane: "passthrough".into(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: cost,
            request_id: self.request_id.clone(),
            status: self.status.to_string(),
            op: "chat".into(),
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
