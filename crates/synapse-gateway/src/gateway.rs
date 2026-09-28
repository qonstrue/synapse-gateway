//! In-process LLM gateway: routing, fallback, native Vertex, ledger, metrics.
//! Transport-independent core; the axum HTTP layer (`server`) delegates here.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use chrono::Utc;
use futures::stream::{BoxStream, Stream};
use tap::Tap;
use uuid::Uuid;

use crate::error::{GatewayError, LegFailure};
use crate::guard::GuardEngine;
use crate::jev_native::JevNativeProvider;
use crate::ledger::{LedgerHandle, UsageEntry};
use crate::observability::GenAiSpan;
use crate::pricing::PricingTable;
use crate::providers::Catalog;
use crate::routing::classify::{classify, vertex_triggers, Lane};
use crate::routing::executor::{
    execute_buffered_with_timeouts, CommittedStream, Completion, LegError, StreamTimeouts,
};
use crate::routing::request::ChatRequest;
use crate::routing::stream::{Accumulator, FinishReason, StreamItem};
use crate::routing::table::{ChainLeg, RouteTable};
use crate::telemetry::GatewayMetrics;
use crate::vertex_native::VertexNativeProvider;

/// Embeddable gateway handle. Construct with [`Gateway::builder`].
#[derive(Clone)]
pub struct Gateway {
    pub(crate) routes: Arc<RouteTable>,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) pricing: Arc<PricingTable>,
    pub(crate) ledger: LedgerHandle,
    pub(crate) vertex_native: Option<Arc<VertexNativeProvider>>,
    pub(crate) jev_native: Option<Arc<JevNativeProvider>>,
    pub(crate) timeouts: StreamTimeouts,
    pub(crate) default_tenant: String,
    pub(crate) embed_routes: Arc<crate::routing::embeddings::EmbeddingRouteTable>,
    pub(crate) embedders:
        std::collections::HashMap<String, Arc<dyn crate::embeddings::EmbeddingProvider>>,
    pub(crate) embed_default_input_per_mtok: f64,
    pub(crate) guard: Arc<GuardEngine>,
    pub(crate) ai_task_types: Arc<crate::ai_task_type::AiTaskTypeTable>,
    pub(crate) metrics: Arc<GatewayMetrics>,
}

/// Per-call identity for an in-process request (replaces HTTP headers).
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    pub tenant: Option<String>,
    pub workspace: Option<String>,
    /// End-user attribution within the tenant (`x-synapse-user` over HTTP).
    pub user: Option<String>,
    /// Conversation / agent thread (`x-synapse-thread` over HTTP).
    pub thread: Option<String>,
    /// Chat / work message within the thread (`x-synapse-message` over HTTP).
    pub message: Option<String>,
    /// Caller-supplied classification of the work this request serves
    /// (`x-synapse-user-task-type` over HTTP). Free-form and never interpreted
    /// by the gateway; recorded on the ledger row for downstream reporting.
    pub user_task_type: Option<String>,
    /// Overrides the route-alias inference for the AI task type
    /// (`x-synapse-ai-task-type` over HTTP). When `None` or empty, the task type
    /// is inferred from the route alias, falling back to `"simple"`.
    pub ai_task_type: Option<String>,
    /// Correlation id for the ledger row. When `None`, falls back to `message`
    /// (so a chat turn correlates with usage), then a generated UUID.
    /// Embedders (and the HTTP layer) can pass their own so the ledger row and
    /// any client-facing response id share one value.
    pub request_id: Option<String>,
}

impl RequestCtx {
    /// Prefer an explicit `request_id`, else the chat `message` id, else `None`
    /// (callers generate a UUID).
    pub fn resolved_request_id(&self) -> Option<String> {
        self.request_id
            .clone()
            .or_else(|| self.message.clone())
            .filter(|s| !s.is_empty())
    }
}

/// Outcome of running a route's `typesafe` legs on the Jev lane.
pub(crate) enum JevAttempt {
    /// Jev answered; the completion's content is the JSON-encoded `answers`
    /// map, and `answers` is the parsed map (hybrid extraction reads it).
    Decided {
        completion: Completion,
        answers: serde_json::Value,
    },
    /// Every typesafe leg failed retryably — the caller falls through to
    /// the remaining legs on the standard/native machinery.
    Exhausted(Vec<LegFailure>),
}

/// Result of a buffered chat call. `Plain` is a normal chat completion;
/// `Hybrid` is a Jev hybrid-extraction response (envelope per the spec).
#[derive(Debug)]
pub enum ChatOutcome {
    Plain(Completion),
    Hybrid(HybridOutcome),
}

/// A completed Jev hybrid extraction: Jev's answers plus per-survivor
/// extraction results. Rendered by `server::hybrid_json`.
#[derive(Debug)]
pub struct HybridOutcome {
    /// Jev build that answered, or `jev-latest` when the lane was exhausted.
    pub model: String,
    /// All Jev answers verbatim (`{}` when degraded).
    pub answers: serde_json::Value,
    /// Candidate keys that met the floor (all candidates when degraded).
    pub survivors: Vec<String>,
    /// Jev lane exhausted, or at least one survivor failed its legs.
    pub degraded: bool,
    /// True when at least one survivor extraction was attempted.
    pub extraction_ran: bool,
    /// Candidate key → extraction result (failed survivors omitted).
    pub extractions: serde_json::Map<String, serde_json::Value>,
    /// Jev + extraction token totals.
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The attribution fields a ledger row carries, resolved once per request.
///
/// Grouped so the streaming and passthrough meters take a single named argument
/// instead of six positional `Option<String>`s, where a swapped pair would
/// compile cleanly and silently mis-attribute usage.
#[derive(Debug, Clone)]
pub struct Attribution {
    pub workspace: Option<String>,
    pub user: Option<String>,
    pub thread: Option<String>,
    pub message: Option<String>,
    pub user_task_type: Option<String>,
    /// Always resolved — see [`Gateway::ai_task_type_of`].
    pub ai_task_type: String,
}

impl Default for Attribution {
    fn default() -> Self {
        Self {
            workspace: None,
            user: None,
            thread: None,
            message: None,
            user_task_type: None,
            ai_task_type: crate::ai_task_type::DEFAULT_AI_TASK_TYPE.to_string(),
        }
    }
}

#[derive(Default)]
pub struct GatewayBuilder {
    routes: Option<RouteTable>,
    catalog: Option<Catalog>,
    pricing: Option<PricingTable>,
    ledger: Option<LedgerHandle>,
    vertex_native: Option<VertexNativeProvider>,
    jev_native: Option<JevNativeProvider>,
    timeouts: Option<StreamTimeouts>,
    default_tenant: Option<String>,
    embed_routes: Option<crate::routing::embeddings::EmbeddingRouteTable>,
    embedders:
        Option<std::collections::HashMap<String, Arc<dyn crate::embeddings::EmbeddingProvider>>>,
    embed_default_input_per_mtok: Option<f64>,
    guard: Option<GuardEngine>,
    ai_task_types: Option<crate::ai_task_type::AiTaskTypeTable>,
    metrics: Option<Arc<GatewayMetrics>>,
}

impl Gateway {
    pub fn builder() -> GatewayBuilder {
        GatewayBuilder::default()
    }

    /// Client-facing model aliases (for `/v1/models` and discovery).
    pub fn model_aliases(&self) -> Vec<String> {
        self.routes.aliases()
    }

    fn resolve_legs(&self, req: &ChatRequest) -> Result<Vec<ChainLeg>, GatewayError> {
        self.routes
            .legs(&req.model)
            .ok_or_else(|| GatewayError::UnknownModel(req.model.clone()))
            .map(<[ChainLeg]>::to_vec)
    }

    /// Run the route's guardrail policy over the request input. Falls back to
    /// the `default` policy; a no-op when neither is configured.
    fn guard_input(&self, req: &ChatRequest) -> Result<(), GatewayError> {
        let policy = self.routes.policy_of(&req.model).unwrap_or("default");
        self.guard.guard(policy, req)
    }

    fn tenant_of<'a>(&'a self, ctx: &'a RequestCtx) -> &'a str {
        ctx.tenant.as_deref().unwrap_or(&self.default_tenant)
    }

    /// Resolve the AI task type for a call: the caller's header when non-empty,
    /// else the task type configured for `alias`, else `"simple"`.
    pub(crate) fn ai_task_type_of(&self, ctx: &RequestCtx, alias: &str) -> String {
        ctx.ai_task_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.ai_task_types.resolve(alias))
            .to_string()
    }

    /// Everything a ledger row needs from the request, resolved once so every
    /// record site (buffered, streaming, embedding, passthrough) agrees.
    pub(crate) fn attribution_of(&self, ctx: &RequestCtx, alias: &str) -> Attribution {
        Attribution {
            workspace: ctx.workspace.clone(),
            user: ctx.user.clone(),
            thread: ctx.thread.clone(),
            message: ctx.message.clone(),
            user_task_type: ctx.user_task_type.clone(),
            ai_task_type: self.ai_task_type_of(ctx, alias),
        }
    }

    /// Fire cost + ledger + metrics for a completed call (buffered side-effects).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        ctx: &RequestCtx,
        route: &str,
        lane: &'static str,
        request_id: &str,
        c: &Completion,
        legs: u32,
        started: Instant,
    ) {
        let cost = self
            .pricing
            .cost_usd(&c.provider, &c.model, c.input_tokens, c.output_tokens);
        let attr = self.attribution_of(ctx, route);
        self.ledger.enqueue(UsageEntry {
            ts: Utc::now(),
            tenant: self.tenant_of(ctx).to_string(),
            workspace: attr.workspace,
            user: attr.user,
            thread: attr.thread,
            message: attr.message,
            route: route.to_string(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            lane: lane.to_string(),
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            cost_usd: cost,
            request_id: request_id.to_string(),
            status: "ok".into(),
            op: "chat".into(),
            user_task_type: attr.user_task_type,
            ai_task_type: attr.ai_task_type,
        });
        let span_lane = match lane {
            "native" => Lane::NativeVertex,
            "jev" => Lane::Jev,
            _ => Lane::Standard,
        };
        GenAiSpan::from_completion(
            c,
            span_lane,
            route,
            self.tenant_of(ctx),
            ctx.workspace.as_deref(),
            legs,
            false,
        )
        .emit_metrics(&self.metrics, started.elapsed().as_secs_f64());
    }

    /// Resolve the native Vertex committed stream (shared by chat/chat_stream).
    ///
    /// Commit the first Vertex leg that can open a stream. On retryable upstream
    /// failures (5xx, 429, 408, transport), advance to the next `provider=vertex`
    /// leg. Non-retryable 4xx abort immediately. Non-vertex legs are skipped
    /// (native features cannot be expressed on other providers).
    ///
    /// NOTE: bounded only by the reqwest client timeout (`config.request_timeout`);
    /// the first-chunk/idle `StreamTimeouts` are not applied here yet.
    pub(crate) async fn native_committed(
        &self,
        req: &ChatRequest,
        legs: &[ChainLeg],
    ) -> Result<CommittedStream, GatewayError> {
        let provider = self
            .vertex_native
            .as_ref()
            .ok_or_else(|| GatewayError::BadRequest("native vertex lane not configured".into()))?;

        let vertex_legs: Vec<&ChainLeg> = legs.iter().filter(|l| l.provider == "vertex").collect();
        if vertex_legs.is_empty() {
            return Err(GatewayError::NativeFeatureUnsupported {
                feature: "native-vertex".into(),
                route: req.model.clone(),
            });
        }

        let mut failures: Vec<LegFailure> = Vec::new();
        let mut last_retryable: Option<GatewayError> = None;
        for leg in &vertex_legs {
            match provider
                .stream_generate(&leg.model, req, leg.region.as_deref())
                .await
            {
                Ok(stream) => {
                    return Ok(CommittedStream::single(
                        "vertex".into(),
                        leg.model.clone(),
                        stream,
                    ));
                }
                Err(e) if native_start_retryable(&e) => {
                    failures.push(LegFailure {
                        provider: leg.provider.clone(),
                        model: leg.model.clone(),
                        message: e.to_string(),
                    });
                    last_retryable = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(
            last_retryable.unwrap_or_else(|| GatewayError::AllLegsFailed {
                route: req.model.clone(),
                failures,
            }),
        )
    }

    /// Run the route's `typesafe` legs against TypeSafe System One (Jev):
    /// state + typed questions in, structured decisions out. Non-retryable
    /// upstream failures (malformed questions, 4xx other than 429/408) abort —
    /// a chat fallback cannot fix them. Retryable failures (429/408/5xx,
    /// transport) advance to the next typesafe leg, then report `Exhausted`
    /// so the caller can fall back to the route's chat legs.
    pub(crate) async fn jev_attempt(
        &self,
        req: &ChatRequest,
        legs: &[ChainLeg],
    ) -> Result<JevAttempt, GatewayError> {
        let jev_legs: Vec<&ChainLeg> = legs.iter().filter(|l| l.provider == "typesafe").collect();
        if jev_legs.is_empty() {
            return Ok(JevAttempt::Exhausted(Vec::new()));
        }
        let ext = req
            .jev
            .as_ref()
            .filter(|j| !j.questions.is_empty())
            .ok_or_else(|| {
                GatewayError::BadRequest(format!(
                    "route '{}' uses provider 'typesafe'; send a 'jev' extension block with questions",
                    req.model
                ))
            })?;
        let provider = self.jev_native.as_ref().ok_or_else(|| {
            GatewayError::BadRequest(
                "typesafe legs require TYPESAFE_API_KEY to be configured".into(),
            )
        })?;

        let state = ext
            .state
            .clone()
            .or_else(|| serde_json::to_string(&req.messages).ok())
            .unwrap_or_default();
        let mut body = serde_json::json!({
            "state": state,
            "questions": ext.questions.clone(),
        });

        let mut failures: Vec<LegFailure> = Vec::new();
        for leg in jev_legs {
            let model = if leg.model.is_empty() {
                crate::jev_native::DEFAULT_MODEL.to_string()
            } else {
                leg.model.clone()
            };
            body["model"] = serde_json::Value::String(model.clone());

            let resp = match provider.evaluate(body.clone()).await {
                Ok(r) => r,
                Err(e) if native_start_retryable(&e) => {
                    failures.push(LegFailure {
                        provider: leg.provider.clone(),
                        model,
                        message: e.to_string(),
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };

            let status = resp.status();
            let bytes = resp.bytes().await.map_err(|e| GatewayError::Upstream {
                status: 502,
                body: e.to_string(),
            })?;
            if status.is_success() {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| GatewayError::Upstream {
                        status: 502,
                        body: format!("jev response is not JSON: {e}"),
                    })?;
                return Ok(JevAttempt::Decided {
                    answers: value["answers"].clone(),
                    completion: Completion {
                        provider: "typesafe".into(),
                        model: value["model"].as_str().unwrap_or(&model).to_string(),
                        content: serde_json::to_string(&value["answers"])
                            .unwrap_or_else(|_| "{}".into()),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0),
                    },
                });
            }

            let message = String::from_utf8_lossy(&bytes).into_owned();
            if status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
            {
                failures.push(LegFailure {
                    provider: leg.provider.clone(),
                    model,
                    message: format!("{status}: {message}"),
                });
                continue;
            }
            return Err(GatewayError::BadRequest(format!(
                "typesafe {}: {message}",
                status.as_u16()
            )));
        }
        Ok(JevAttempt::Exhausted(failures))
    }

    /// Jev answers are unary: present a buffered decision as a single content
    /// delta + terminal item so the streaming surface renders one SSE chunk.
    fn jev_committed(c: Completion) -> CommittedStream {
        let items: Vec<Result<StreamItem, LegError>> = vec![
            Ok(StreamItem::Delta(c.content.clone())),
            Ok(StreamItem::Done {
                input_tokens: c.input_tokens,
                output_tokens: c.output_tokens,
                finish_reason: c.finish_reason,
            }),
        ];
        CommittedStream::single("typesafe".into(), c.model, futures::stream::iter(items))
    }

    /// Streaming in-process chat: commit a leg on the first item; returns a
    /// `GuardedStream` that yields items and fires side-effects on completion/drop.
    pub async fn chat_stream(
        &self,
        req: ChatRequest,
        ctx: &RequestCtx,
    ) -> Result<GuardedStream, GatewayError> {
        use crate::routing::executor::execute_streaming_with_timeouts;
        let started = Instant::now();
        let legs = self.resolve_legs(&req)?;
        self.guard_input(&req)?;
        let request_id = ctx
            .resolved_request_id()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let vertex_leg_count = legs.iter().filter(|l| l.provider == "vertex").count() as u32;
        require_jev_block(&req, &legs)?;
        if let Err(message) = crate::routing::jev_extract::validate_extract(
            &req,
            legs.iter().any(|l| l.provider != "typesafe"),
            legs.iter().any(|l| l.provider == "vertex"),
        ) {
            return Err(GatewayError::BadRequest(message));
        }
        let (committed, lane_str, legs_attempted) = match classify(&req) {
            Lane::Standard => (
                execute_streaming_with_timeouts(
                    &self.catalog,
                    &req.model,
                    &legs,
                    &req,
                    self.timeouts,
                )
                .await?,
                "standard",
                legs.len() as u32,
            ),
            Lane::NativeVertex => (
                self.native_committed(&req, &legs).await?,
                "native",
                vertex_leg_count.max(1),
            ),
            Lane::Jev => match self.jev_attempt(&req, &legs).await? {
                JevAttempt::Decided { completion, .. } => (
                    Self::jev_committed(completion),
                    "jev",
                    legs.iter()
                        .filter(|l| l.provider == "typesafe")
                        .count()
                        .max(1) as u32,
                ),
                JevAttempt::Exhausted(failures) => {
                    let rest: Vec<ChainLeg> = non_typesafe_legs(&legs);
                    if rest.is_empty() {
                        return Err(GatewayError::AllLegsFailed {
                            route: req.model.clone(),
                            failures,
                        });
                    }
                    if vertex_triggers(&req) {
                        let vertex_n =
                            rest.iter().filter(|l| l.provider == "vertex").count() as u32;
                        (
                            self.native_committed(&req, &rest).await?,
                            "native",
                            vertex_n.max(1),
                        )
                    } else {
                        (
                            execute_streaming_with_timeouts(
                                &self.catalog,
                                &req.model,
                                &rest,
                                &req,
                                self.timeouts,
                            )
                            .await?,
                            "standard",
                            rest.len() as u32,
                        )
                    }
                }
            },
        };
        let model = committed.model.clone();
        let guard = StreamSideEffects::new(
            self.ledger.clone(),
            self.pricing.clone(),
            self.metrics.clone(),
            req.model.clone(),
            self.tenant_of(ctx).to_string(),
            self.attribution_of(ctx, &req.model),
            committed.provider.clone(),
            committed.model.clone(),
            lane_str,
            request_id,
            legs_attempted,
            started,
        );
        Ok(GuardedStream::new(committed.stream, model, guard))
    }

    /// Buffered in-process chat: stream the chain internally, aggregate, fire
    /// side-effects, return a `ChatOutcome` (plain or hybrid).
    pub async fn chat(
        &self,
        req: ChatRequest,
        ctx: &RequestCtx,
    ) -> Result<ChatOutcome, GatewayError> {
        let started = Instant::now();
        let legs = self.resolve_legs(&req)?;
        self.guard_input(&req)?;
        let request_id = ctx
            .resolved_request_id()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        require_jev_block(&req, &legs)?;
        if let Err(message) = crate::routing::jev_extract::validate_extract(
            &req,
            legs.iter().any(|l| l.provider != "typesafe"),
            legs.iter().any(|l| l.provider == "vertex"),
        ) {
            return Err(GatewayError::BadRequest(message));
        }
        if req.jev.as_ref().and_then(|j| j.extract.as_ref()).is_some() {
            let outcome = self
                .chat_hybrid(req, ctx, &legs, started, &request_id)
                .await?;
            return Ok(ChatOutcome::Hybrid(outcome));
        }
        let (completion, lane_str, legs_n) = match classify(&req) {
            Lane::Standard => execute_buffered_with_timeouts(
                &self.catalog,
                &req.model,
                &legs,
                &req,
                self.timeouts,
            )
            .await
            .map(|c| (c, "standard", legs.len() as u32))?,
            Lane::NativeVertex => {
                let vertex_n = legs.iter().filter(|l| l.provider == "vertex").count() as u32;
                let committed = self.native_committed(&req, &legs).await?;
                (
                    collect_committed(committed).await?,
                    "native",
                    vertex_n.max(1),
                )
            }
            Lane::Jev => match self.jev_attempt(&req, &legs).await? {
                JevAttempt::Decided { completion, .. } => (
                    completion,
                    "jev",
                    legs.iter()
                        .filter(|l| l.provider == "typesafe")
                        .count()
                        .max(1) as u32,
                ),
                JevAttempt::Exhausted(failures) => {
                    let rest: Vec<ChainLeg> = non_typesafe_legs(&legs);
                    if rest.is_empty() {
                        return Err(GatewayError::AllLegsFailed {
                            route: req.model.clone(),
                            failures,
                        });
                    }
                    if vertex_triggers(&req) {
                        let committed = self.native_committed(&req, &rest).await?;
                        let vertex_n =
                            rest.iter().filter(|l| l.provider == "vertex").count() as u32;
                        (
                            collect_committed(committed).await?,
                            "native",
                            vertex_n.max(1),
                        )
                    } else {
                        execute_buffered_with_timeouts(
                            &self.catalog,
                            &req.model,
                            &rest,
                            &req,
                            self.timeouts,
                        )
                        .await
                        .map(|c| (c, "standard", rest.len() as u32))?
                    }
                }
            },
        };
        self.record(
            ctx,
            &req.model,
            lane_str,
            &request_id,
            &completion,
            legs_n,
            started,
        );
        Ok(ChatOutcome::Plain(completion))
    }

    /// Embed `req.input` against the embedding alias `req.model`, pinning output to
    /// the alias's declared dimension. Tries each leg in order, falling through on
    /// upstream error (simple ordered fallback). Records one ledger row on success.
    ///
    // TODO(embeddings): wrap legs in resilience breakers like the chat path.
    pub async fn embed(
        &self,
        req: crate::embeddings::EmbeddingRequest,
        ctx: RequestCtx,
    ) -> Result<crate::embeddings::EmbeddingResponse, GatewayError> {
        let alias = req.model.clone();
        let dims = self
            .embed_routes
            .dimensions(&alias)
            .ok_or_else(|| GatewayError::UnknownModel(alias.clone()))?;
        if let Some(d) = req.dimensions {
            if d != dims {
                return Err(GatewayError::BadRequest(format!(
                    "dimensions {d} does not match embedding alias '{alias}' dimension {dims}"
                )));
            }
        }
        let inputs = req.input.into_vec();
        let legs = self
            .embed_routes
            .legs(&alias)
            .ok_or_else(|| GatewayError::UnknownModel(alias.clone()))?
            .to_vec();

        let mut last_err: Option<GatewayError> = None;
        for leg in &legs {
            let Some(embedder) = self.embedders.get(&leg.provider) else {
                continue;
            };
            let limit = match leg.provider.as_str() {
                "vertex" => crate::embeddings::vertex::VERTEX_EMBED_BATCH,
                _ => crate::embeddings::openai::OPENAI_EMBED_BATCH,
            };
            let started = std::time::Instant::now();
            match self
                .embed_all_batches(embedder.as_ref(), &leg.model, &inputs, dims, limit)
                .await
            {
                Ok(out) => {
                    self.metrics.embedding(
                        &alias,
                        &leg.model,
                        &leg.provider,
                        started.elapsed().as_secs_f64(),
                    );
                    self.record_embed_usage(&ctx, &alias, leg, out.input_tokens);
                    return Ok(crate::embeddings::build_response(alias, out));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(GatewayError::AllLegsFailed {
            route: alias,
            failures: Vec::new(),
        }))
    }

    /// Embed `inputs` in provider-sized batches, concatenating vectors (input order)
    /// and summing input tokens into a single `EmbedOut`.
    async fn embed_all_batches(
        &self,
        embedder: &dyn crate::embeddings::EmbeddingProvider,
        model: &str,
        inputs: &[String],
        dims: u32,
        limit: usize,
    ) -> Result<crate::embeddings::EmbedOut, GatewayError> {
        let mut vectors = Vec::with_capacity(inputs.len());
        let mut input_tokens = 0u64;
        for batch in crate::embeddings::split_batches(inputs, limit) {
            let out = embedder.embed(model, batch, dims).await?;
            input_tokens += out.input_tokens;
            vectors.extend(out.vectors);
        }
        Ok(crate::embeddings::EmbedOut {
            vectors,
            input_tokens,
        })
    }

    /// Fire one cost + ledger row for a completed embedding call (input tokens only).
    fn record_embed_usage(&self, ctx: &RequestCtx, alias: &str, leg: &ChainLeg, input_tokens: u64) {
        let cost = self.pricing.embedding_cost_usd(
            &leg.provider,
            &leg.model,
            input_tokens,
            self.embed_default_input_per_mtok,
        );
        let attr = self.attribution_of(ctx, alias);
        self.ledger.enqueue(UsageEntry {
            ts: Utc::now(),
            tenant: self.tenant_of(ctx).to_string(),
            workspace: attr.workspace,
            user: attr.user,
            thread: attr.thread,
            message: attr.message,
            route: alias.to_string(),
            provider: leg.provider.clone(),
            model: leg.model.clone(),
            lane: "embedding".into(),
            input_tokens,
            output_tokens: 0,
            cost_usd: cost,
            request_id: ctx
                .resolved_request_id()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            status: "ok".into(),
            op: "embedding".into(),
            user_task_type: attr.user_task_type,
            ai_task_type: attr.ai_task_type,
        });
    }
}

/// True when a native-lane stream **start** failure should advance to the next
/// Vertex leg (aligned with standard-lane / reqwest retryable policy).
fn native_start_retryable(e: &GatewayError) -> bool {
    match e {
        GatewayError::Upstream { status, .. } => {
            *status >= 500
                || *status == reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16()
                || *status == reqwest::StatusCode::REQUEST_TIMEOUT.as_u16()
        }
        GatewayError::UpstreamTimeout => true,
        _ => false,
    }
}

/// Routes with `typesafe` legs answer typed questions, not chat: refuse with a
/// clear 400 when the request carries no `jev` extension block, instead of
/// letting the standard executor fail on a provider it has no client for.
fn require_jev_block(req: &ChatRequest, legs: &[ChainLeg]) -> Result<(), GatewayError> {
    let needs_block = legs.iter().any(|l| l.provider == "typesafe");
    let has_block = req.jev.as_ref().is_some_and(|j| !j.questions.is_empty());
    match (needs_block, has_block) {
        (true, false) => Err(GatewayError::BadRequest(format!(
            "route '{}' uses provider 'typesafe'; send a 'jev' extension block with questions",
            req.model
        ))),
        _ => Ok(()),
    }
}

/// Legs remaining after the Jev lane has consumed the `typesafe` ones: the
/// fallback chain when every Jev leg fails retryably.
fn non_typesafe_legs(legs: &[ChainLeg]) -> Vec<ChainLeg> {
    legs.iter()
        .filter(|l| l.provider != "typesafe")
        .cloned()
        .collect()
}

impl GatewayBuilder {
    pub fn routes(mut self, routes: RouteTable) -> Self {
        self.routes = Some(routes);
        self
    }
    pub fn catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }
    pub fn pricing(mut self, pricing: PricingTable) -> Self {
        self.pricing = Some(pricing);
        self
    }
    pub fn ledger(mut self, ledger: LedgerHandle) -> Self {
        self.ledger = Some(ledger);
        self
    }
    pub fn vertex_native(mut self, v: Option<VertexNativeProvider>) -> Self {
        self.vertex_native = v;
        self
    }
    pub fn jev_native(mut self, v: Option<JevNativeProvider>) -> Self {
        self.jev_native = v;
        self
    }
    pub fn timeouts(mut self, t: StreamTimeouts) -> Self {
        self.timeouts = Some(t);
        self
    }
    pub fn default_tenant(mut self, t: impl Into<String>) -> Self {
        self.default_tenant = Some(t.into());
        self
    }
    pub fn embed_routes(mut self, t: crate::routing::embeddings::EmbeddingRouteTable) -> Self {
        self.embed_routes = Some(t);
        self
    }
    pub fn embedder(
        mut self,
        id: impl Into<String>,
        e: Arc<dyn crate::embeddings::EmbeddingProvider>,
    ) -> Self {
        self.embedders
            .get_or_insert_with(Default::default)
            .insert(id.into(), e);
        self
    }
    pub fn embed_default_input_per_mtok(mut self, v: f64) -> Self {
        self.embed_default_input_per_mtok = Some(v);
        self
    }
    pub fn guard(mut self, guard: GuardEngine) -> Self {
        self.guard = Some(guard);
        self
    }
    pub fn ai_task_types(mut self, t: crate::ai_task_type::AiTaskTypeTable) -> Self {
        self.ai_task_types = Some(t);
        self
    }
    /// Where every gateway metric is recorded. Defaults to a no-op. The guard
    /// engine always records here, whatever was attached to it before.
    pub fn metrics(mut self, metrics: Arc<GatewayMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn build(self) -> anyhow::Result<Gateway> {
        let metrics = self.metrics.unwrap_or_else(GatewayMetrics::noop);
        Ok(Gateway {
            routes: Arc::new(
                self.routes
                    .ok_or_else(|| anyhow::anyhow!("Gateway: routes required"))?,
            ),
            catalog: Arc::new(
                self.catalog
                    .ok_or_else(|| anyhow::anyhow!("Gateway: catalog required"))?
                    .tap(|c| c.attach_metrics(&metrics)),
            ),
            pricing: Arc::new(
                self.pricing
                    .ok_or_else(|| anyhow::anyhow!("Gateway: pricing required"))?,
            ),
            ledger: self
                .ledger
                .ok_or_else(|| anyhow::anyhow!("Gateway: ledger required"))?,
            vertex_native: self.vertex_native.map(Arc::new),
            jev_native: self.jev_native.map(Arc::new),
            timeouts: self.timeouts.unwrap_or_default(),
            default_tenant: self.default_tenant.unwrap_or_else(|| "unattributed".into()),
            embed_routes: Arc::new(self.embed_routes.unwrap_or_default()),
            embedders: self.embedders.unwrap_or_default(),
            embed_default_input_per_mtok: self.embed_default_input_per_mtok.unwrap_or(0.10),
            guard: Arc::new(
                self.guard
                    .unwrap_or_else(GuardEngine::empty)
                    .with_metrics(metrics.clone()),
            ),
            ai_task_types: Arc::new(self.ai_task_types.unwrap_or_default()),
            metrics,
        })
    }
}

/// Accumulates running usage during a streamed response and fires cost, ledger,
/// and metrics exactly once when dropped (normal end, error, or client disconnect).
/// Emitting from `Drop` guarantees the side-effects run on every termination path,
/// including client disconnect where the response future is cancelled.
pub(crate) struct StreamSideEffects {
    ledger: LedgerHandle,
    pricing: Arc<PricingTable>,
    metrics: Arc<GatewayMetrics>,
    route: String,
    tenant: String,
    attribution: Attribution,
    provider: String,
    model: String,
    lane: &'static str, // "standard" | "native"
    request_id: String,
    legs_attempted: u32,
    started: Instant,
    input_tokens: u64,
    output_tokens: u64,
    status: &'static str,
    fired: bool,
}

impl StreamSideEffects {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ledger: LedgerHandle,
        pricing: Arc<PricingTable>,
        metrics: Arc<GatewayMetrics>,
        route: String,
        tenant: String,
        attribution: Attribution,
        provider: String,
        model: String,
        lane: &'static str,
        request_id: String,
        legs_attempted: u32,
        started: Instant,
    ) -> Self {
        Self {
            ledger,
            pricing,
            metrics,
            route,
            tenant,
            attribution,
            provider,
            model,
            lane,
            request_id,
            legs_attempted,
            started,
            input_tokens: 0,
            output_tokens: 0,
            status: "ok",
            fired: false,
        }
    }

    /// Fold a streamed item into the running usage totals.
    pub(crate) fn observe(&mut self, item: &StreamItem) {
        if let StreamItem::Done {
            input_tokens,
            output_tokens,
            ..
        } = item
        {
            self.input_tokens = *input_tokens;
            self.output_tokens = *output_tokens;
        }
    }

    /// Mark the response as failed so the ledger row records `status = "error"`.
    pub(crate) fn mark_error(&mut self) {
        self.status = "error";
    }
}

impl Drop for StreamSideEffects {
    fn drop(&mut self) {
        if self.fired {
            return;
        }
        self.fired = true;

        // Cost + ledger (fire-and-forget), mirroring the buffered handler.
        let cost = self.pricing.cost_usd(
            &self.provider,
            &self.model,
            self.input_tokens,
            self.output_tokens,
        );
        self.ledger.enqueue(UsageEntry {
            ts: Utc::now(),
            tenant: self.tenant.clone(),
            workspace: self.attribution.workspace.clone(),
            user: self.attribution.user.clone(),
            thread: self.attribution.thread.clone(),
            message: self.attribution.message.clone(),
            route: self.route.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            lane: self.lane.to_string(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: cost,
            request_id: self.request_id.clone(),
            status: self.status.to_string(),
            op: "chat".into(),
            user_task_type: self.attribution.user_task_type.clone(),
            ai_task_type: self.attribution.ai_task_type.clone(),
        });

        // Metrics via GenAiSpan for parity with the buffered path: build a minimal
        // Completion from the running fields and emit with `stream: true`.
        let completion = Completion {
            provider: self.provider.clone(),
            model: self.model.clone(),
            content: String::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        };
        let lane = if self.lane == "native" {
            Lane::NativeVertex
        } else {
            Lane::Standard
        };
        GenAiSpan::from_completion(
            &completion,
            lane,
            &self.route,
            &self.tenant,
            self.attribution.workspace.as_deref(),
            self.legs_attempted,
            true,
        )
        .emit_metrics(&self.metrics, self.started.elapsed().as_secs_f64());
    }
}

/// A streaming response that yields normalized `StreamItem`s and fires
/// cost/ledger/metrics exactly once when fully consumed OR dropped early
/// (via the owned `StreamSideEffects` guard). Transport-independent.
pub struct GuardedStream {
    inner: BoxStream<'static, Result<StreamItem, LegError>>,
    model: String,
    guard: StreamSideEffects,
}

impl GuardedStream {
    pub(crate) fn new(
        inner: BoxStream<'static, Result<StreamItem, LegError>>,
        model: String,
        guard: StreamSideEffects,
    ) -> Self {
        Self {
            inner,
            model,
            guard,
        }
    }

    /// The model id of the committed leg (for the OpenAI chunk `model` field).
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Stream for GuardedStream {
    type Item = Result<StreamItem, LegError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Both fields are Unpin (BoxStream is Pin<Box<..>>; StreamSideEffects is plain data).
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                this.guard.observe(&item);
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.guard.mark_error();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Drain a committed stream into a single `Completion`. ROP: map the per-item
/// error onto the failure track, `try_fold` items into an `Accumulator`, then map
/// to a `Completion`.
pub(crate) async fn collect_committed(
    committed: CommittedStream,
) -> Result<Completion, GatewayError> {
    use futures::TryStreamExt;
    let CommittedStream {
        provider,
        model,
        stream,
    } = committed;
    stream
        .map_err(|e: LegError| GatewayError::Upstream {
            status: 502,
            body: e.to_string(),
        })
        .try_fold(Accumulator::default(), |mut acc, item| async move {
            acc.push(item);
            Ok(acc)
        })
        .await
        .map(|acc| Completion {
            provider,
            model,
            content: acc.content,
            tool_calls: acc.tool_calls,
            finish_reason: acc.finish_reason,
            input_tokens: acc.input_tokens,
            output_tokens: acc.output_tokens,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{InMemoryLedger, LedgerHandle, LedgerStore};

    fn test_gateway() -> Gateway {
        let routes = RouteTable::from_toml_str(
            r#"[routes."fast"]
               legs = [{ provider = "qwen", model = "qwen-max" }]"#,
        )
        .unwrap();
        let catalog = Catalog::for_test(vec![("qwen", "http://127.0.0.1:1/v1".into())]);
        let ledger = LedgerHandle::spawn(
            Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>,
            16,
        );
        Gateway::builder()
            .routes(routes)
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(ledger)
            .default_tenant("acme")
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn builder_builds_and_lists_aliases() {
        let gw = test_gateway();
        assert_eq!(gw.model_aliases(), vec!["fast".to_string()]);
        assert_eq!(gw.default_tenant, "acme");
    }

    #[test]
    fn builder_requires_components() {
        assert!(Gateway::builder().build().is_err());
    }

    #[tokio::test]
    async fn guard_records_usage_on_drop() {
        let store = Arc::new(InMemoryLedger::default());
        let ledger = LedgerHandle::spawn(store.clone(), 16);
        let pricing = Arc::new(PricingTable::default());
        {
            let mut guard = StreamSideEffects::new(
                ledger.clone(),
                pricing,
                GatewayMetrics::noop(),
                "route".into(),
                "tenant".into(),
                Attribution::default(),
                "p".into(),
                "m".into(),
                "standard",
                "rid".into(),
                1,
                Instant::now(),
            );
            guard.observe(&crate::routing::stream::StreamItem::Done {
                input_tokens: 3,
                output_tokens: 2,
                finish_reason: crate::routing::stream::FinishReason::Stop,
            });
        } // drop -> records
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 3);
        assert_eq!(rows[0].output_tokens, 2);
        assert_eq!(rows[0].status, "ok");
    }

    #[tokio::test]
    async fn guarded_stream_yields_items_and_records_on_drop() {
        use crate::routing::stream::{FinishReason, StreamItem};
        use futures::StreamExt;
        let store = Arc::new(InMemoryLedger::default());
        let ledger = LedgerHandle::spawn(store.clone() as Arc<dyn LedgerStore>, 16);
        let inner = futures::stream::iter(vec![
            Ok(StreamItem::Delta("hi".into())),
            Ok(StreamItem::Done {
                input_tokens: 4,
                output_tokens: 2,
                finish_reason: FinishReason::Stop,
            }),
        ])
        .boxed();
        let guard = StreamSideEffects::new(
            ledger,
            Arc::new(PricingTable::default()),
            GatewayMetrics::noop(),
            "route".into(),
            "acme".into(),
            Attribution::default(),
            "p".into(),
            "m".into(),
            "standard",
            "rid".into(),
            1,
            std::time::Instant::now(),
        );
        {
            let mut gs = GuardedStream::new(inner, "m".into(), guard);
            let mut n = 0;
            while let Some(item) = gs.next().await {
                item.unwrap();
                n += 1;
            }
            assert_eq!(n, 2);
            assert_eq!(gs.model(), "m");
        } // drop -> records
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 4);
        assert_eq!(rows[0].tenant, "acme");
    }

    /// Minimal gateway for exercising resolution logic (no upstream needed).
    fn resolver_gateway(ai_task_types: &str) -> Gateway {
        let routes = RouteTable::from_toml_str(
            "[routes.\"fast\"]\nlegs = [{ provider = \"qwen\", model = \"qwen-max\" }]",
        )
        .unwrap();
        Gateway::builder()
            .routes(routes)
            .catalog(Catalog::for_test(vec![(
                "qwen",
                "http://127.0.0.1:1/v1".into(),
            )]))
            .pricing(PricingTable::default())
            .ledger(LedgerHandle::spawn(
                Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>,
                4,
            ))
            .ai_task_types(
                crate::ai_task_type::AiTaskTypeTable::from_toml_str(ai_task_types).unwrap(),
            )
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn ai_task_type_prefers_the_caller_header_over_the_alias_mapping() {
        let gw = resolver_gateway("conversation = [\"fast\"]");
        let ctx = RequestCtx {
            ai_task_type: Some("caller-supplied".into()),
            ..Default::default()
        };
        assert_eq!(gw.ai_task_type_of(&ctx, "fast"), "caller-supplied");
    }

    #[tokio::test]
    async fn ai_task_type_is_inferred_from_the_route_alias_when_no_header() {
        let gw = resolver_gateway("conversation = [\"fast\", \"planning\"]");
        let ctx = RequestCtx::default();
        assert_eq!(gw.ai_task_type_of(&ctx, "fast"), "conversation");
        assert_eq!(gw.ai_task_type_of(&ctx, "planning"), "conversation");
    }

    #[tokio::test]
    async fn ai_task_type_defaults_to_simple_for_an_unmapped_alias() {
        let gw = resolver_gateway("conversation = [\"fast\"]");
        assert_eq!(
            gw.ai_task_type_of(&RequestCtx::default(), "graph-llm"),
            "simple"
        );
    }

    #[tokio::test]
    async fn an_empty_ai_task_type_header_falls_back_to_inference() {
        let gw = resolver_gateway("conversation = [\"fast\"]");
        let ctx = RequestCtx {
            ai_task_type: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(gw.ai_task_type_of(&ctx, "fast"), "conversation");
    }

    #[tokio::test]
    async fn chat_returns_completion_and_records_ledger() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                                  data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
                                  data: [DONE]\n\n"))
            .mount(&mock).await;
        let routes = RouteTable::from_toml_str(
            "[routes.\"fast\"]\nlegs = [{ provider = \"qwen\", model = \"qwen-max\" }]",
        )
        .unwrap();
        let catalog = Catalog::for_test(vec![("qwen", format!("{}/v1", mock.uri()))]);
        let store = Arc::new(InMemoryLedger::default());
        let ledger = LedgerHandle::spawn(store.clone() as Arc<dyn LedgerStore>, 16);
        let gw = Gateway::builder()
            .routes(routes)
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(ledger)
            .default_tenant("def")
            .ai_task_types(
                crate::ai_task_type::AiTaskTypeTable::from_toml_str("conversation = [\"fast\"]")
                    .unwrap(),
            )
            .build()
            .unwrap();
        let req = serde_json::from_value(serde_json::json!(
            {"model":"fast","messages":[{"role":"user","content":"hi"}]}))
        .unwrap();
        let ctx = RequestCtx {
            tenant: Some("acme".into()),
            workspace: Some("ws-9".into()),
            user: Some("user-42".into()),
            thread: Some("thread-9".into()),
            message: Some("msg-7".into()),
            user_task_type: Some("summarisation".into()),
            // No override: the ledger row must show the alias-inferred value.
            ai_task_type: None,
            request_id: Some("corr-123".into()),
        };
        let c = match gw.chat(req, &ctx).await.unwrap() {
            ChatOutcome::Plain(c) => c,
            ChatOutcome::Hybrid(_) => panic!("expected plain completion"),
        };
        assert_eq!(c.content, "hi");
        assert_eq!(c.input_tokens, 3);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tenant, "acme");
        assert_eq!(rows[0].user.as_deref(), Some("user-42"));
        assert_eq!(rows[0].thread.as_deref(), Some("thread-9"));
        assert_eq!(rows[0].message.as_deref(), Some("msg-7"));
        assert_eq!(rows[0].user_task_type.as_deref(), Some("summarisation"));
        // No x-synapse-ai-task-type header: inferred from the "fast" route alias.
        assert_eq!(rows[0].ai_task_type, "conversation");
        // A caller-supplied request_id propagates to the ledger row.
        assert_eq!(rows[0].request_id, "corr-123");
    }

    #[tokio::test]
    async fn chat_stream_yields_items_and_records() {
        use crate::routing::stream::StreamItem;
        use futures::StreamExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: {\"choices\":[{\"delta\":{\"content\":\"go\"}}]}\n\n\
                                  data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
                                  data: [DONE]\n\n"))
            .mount(&mock).await;
        let routes = RouteTable::from_toml_str(
            "[routes.\"fast\"]\nlegs = [{ provider = \"qwen\", model = \"qwen-max\" }]",
        )
        .unwrap();
        let catalog = Catalog::for_test(vec![("qwen", format!("{}/v1", mock.uri()))]);
        let store = Arc::new(InMemoryLedger::default());
        let ledger = LedgerHandle::spawn(store.clone() as Arc<dyn LedgerStore>, 16);
        let gw = Gateway::builder()
            .routes(routes)
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(ledger)
            .default_tenant("def")
            .ai_task_types(
                crate::ai_task_type::AiTaskTypeTable::from_toml_str("conversation = [\"fast\"]")
                    .unwrap(),
            )
            .build()
            .unwrap();
        let req = serde_json::from_value(serde_json::json!(
            {"model":"fast","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .unwrap();
        let ctx = RequestCtx {
            user_task_type: Some("code-review".into()),
            ..Default::default()
        };
        let mut stream = gw.chat_stream(req, &ctx).await.unwrap();
        let mut got = false;
        while let Some(i) = stream.next().await {
            if matches!(i.unwrap(), StreamItem::Delta(ref t) if t == "go") {
                got = true;
            }
        }
        drop(stream);
        assert!(got);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_task_type.as_deref(), Some("code-review"));
        assert_eq!(rows[0].ai_task_type, "conversation");
    }

    // --- Embeddings ---------------------------------------------------------

    use crate::embeddings::{EmbedOut, EmbeddingInput, EmbeddingProvider, EmbeddingRequest};
    use async_trait::async_trait;

    /// Always fails — exercises the ordered-fallback path.
    struct FlakyEmbedder;
    #[async_trait]
    impl EmbeddingProvider for FlakyEmbedder {
        async fn embed(
            &self,
            _model: &str,
            _inputs: &[String],
            _dims: u32,
        ) -> Result<EmbedOut, GatewayError> {
            Err(GatewayError::Upstream {
                status: 500,
                body: "x".into(),
            })
        }
    }

    /// Returns one zero-vector per input (length `dims`) and a fixed token count.
    struct GoodEmbedder;
    #[async_trait]
    impl EmbeddingProvider for GoodEmbedder {
        async fn embed(
            &self,
            _model: &str,
            inputs: &[String],
            dims: u32,
        ) -> Result<EmbedOut, GatewayError> {
            Ok(EmbedOut {
                vectors: inputs.iter().map(|_| vec![0.0f32; dims as usize]).collect(),
                input_tokens: 6,
            })
        }
    }

    fn embed_gateway() -> (Gateway, Arc<InMemoryLedger>) {
        let embed_routes = crate::routing::embeddings::EmbeddingRouteTable::from_toml_str(
            r#"
            [embeddings."default-embed"]
            dimensions = 4
            legs = [
              { provider = "flaky", model = "flaky-embed" },
              { provider = "good", model = "good-embed" },
            ]
            "#,
        )
        .unwrap();
        let routes = RouteTable::from_toml_str(
            "[routes.\"fast\"]\nlegs = [{ provider = \"qwen\", model = \"qwen-max\" }]",
        )
        .unwrap();
        let catalog = Catalog::for_test(vec![("qwen", "http://127.0.0.1:1/v1".into())]);
        let store = Arc::new(InMemoryLedger::default());
        let ledger = LedgerHandle::spawn(store.clone() as Arc<dyn LedgerStore>, 16);
        let gw = Gateway::builder()
            .routes(routes)
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(ledger)
            .default_tenant("acme")
            .embed_routes(embed_routes)
            .embedder(
                "flaky",
                Arc::new(FlakyEmbedder) as Arc<dyn EmbeddingProvider>,
            )
            .embedder("good", Arc::new(GoodEmbedder) as Arc<dyn EmbeddingProvider>)
            .build()
            .unwrap();
        (gw, store)
    }

    #[tokio::test]
    async fn embed_falls_through_to_good_leg_and_records_usage() {
        let (gw, store) = embed_gateway();
        let req = EmbeddingRequest {
            input: EmbeddingInput::Many(vec!["a".into(), "b".into()]),
            model: "default-embed".into(),
            dimensions: None,
        };
        let ctx = RequestCtx {
            user_task_type: Some("retrieval".into()),
            ..Default::default()
        };
        let resp = gw.embed(req, ctx).await.unwrap();
        assert_eq!(resp.data.len(), 2);
        assert!(resp.data.iter().all(|d| d.embedding.len() == 4));
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[1].index, 1);
        assert_eq!(resp.usage.prompt_tokens, 6);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].op, "embedding");
        assert_eq!(rows[0].lane, "embedding");
        assert_eq!(rows[0].output_tokens, 0);
        assert_eq!(rows[0].provider, "good");
        assert!(rows[0].cost_usd > 0.0);
        assert_eq!(rows[0].user_task_type.as_deref(), Some("retrieval"));
        assert_eq!(rows[0].ai_task_type, "simple");
    }

    #[tokio::test]
    async fn embed_dimension_mismatch_is_bad_request() {
        let (gw, _store) = embed_gateway();
        let req = EmbeddingRequest {
            input: EmbeddingInput::Many(vec!["a".into()]),
            model: "default-embed".into(),
            dimensions: Some(8),
        };
        let err = gw.embed(req, RequestCtx::default()).await.unwrap_err();
        assert!(matches!(err, GatewayError::BadRequest(_)));
    }

    #[tokio::test]
    async fn chat_blocks_when_route_policy_refuses() {
        use crate::guard::{GuardEngine, GuardrailsConfig};
        let routes = RouteTable::from_toml_str(
            r#"[routes."fast"]
               policy = "strict"
               legs = [{ provider = "qwen", model = "qwen-max" }]"#,
        )
        .unwrap();
        let guard = GuardEngine::from_config(
            &GuardrailsConfig::from_toml_str(
                r#"[guardrails.strict]
                   scanners = [{ type = "ban_substrings", substrings = ["forbidden"] }]"#,
            )
            .unwrap(),
        )
        .unwrap();
        let catalog = Catalog::for_test(vec![("qwen", "http://127.0.0.1:1/v1".into())]);
        let ledger = LedgerHandle::spawn(
            Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>,
            16,
        );
        let gw = Gateway::builder()
            .routes(routes)
            .catalog(catalog)
            .pricing(PricingTable::default())
            .ledger(ledger)
            .guard(guard)
            .build()
            .unwrap();
        let req = serde_json::from_value(serde_json::json!({
            "model": "fast",
            "messages": [{ "role": "user", "content": "this is forbidden" }]
        }))
        .unwrap();
        let err = gw.chat(req, &RequestCtx::default()).await.unwrap_err();
        assert!(matches!(err, GatewayError::ContentBlocked { .. }));
    }
}
