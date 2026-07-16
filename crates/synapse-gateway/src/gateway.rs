//! In-process LLM gateway: routing, fallback, native Vertex, ledger, metrics.
//! Transport-independent core; the axum HTTP layer (`server`) delegates here.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use chrono::Utc;
use futures::stream::{BoxStream, Stream};
use uuid::Uuid;

use crate::error::GatewayError;
use crate::guard::GuardEngine;
use crate::ledger::{LedgerHandle, UsageEntry};
use crate::observability::GenAiSpan;
use crate::pricing::PricingTable;
use crate::providers::Catalog;
use crate::routing::classify::{classify, Lane};
use crate::routing::executor::{
    execute_buffered_with_timeouts, CommittedStream, Completion, LegError, StreamTimeouts,
};
use crate::routing::request::ChatRequest;
use crate::routing::stream::{Accumulator, FinishReason, StreamItem};
use crate::routing::table::{ChainLeg, RouteTable};
use crate::vertex_native::VertexNativeProvider;

/// Embeddable gateway handle. Construct with [`Gateway::builder`].
#[derive(Clone)]
pub struct Gateway {
    pub(crate) routes: Arc<RouteTable>,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) pricing: Arc<PricingTable>,
    pub(crate) ledger: LedgerHandle,
    pub(crate) vertex_native: Option<Arc<VertexNativeProvider>>,
    pub(crate) timeouts: StreamTimeouts,
    pub(crate) default_tenant: String,
    pub(crate) embed_routes: Arc<crate::routing::embeddings::EmbeddingRouteTable>,
    pub(crate) embedders:
        std::collections::HashMap<String, Arc<dyn crate::embeddings::EmbeddingProvider>>,
    pub(crate) embed_default_input_per_mtok: f64,
    pub(crate) guard: Arc<GuardEngine>,
}

/// Per-call identity for an in-process request (replaces HTTP headers).
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    pub tenant: Option<String>,
    pub workspace: Option<String>,
    /// End-user attribution within the tenant (`x-synapse-user` over HTTP).
    pub user: Option<String>,
    /// Correlation id for the ledger row. When `None` the gateway generates a
    /// UUID. Embedders (and the HTTP layer) can pass their own so the ledger
    /// row and any client-facing response id share one value.
    pub request_id: Option<String>,
}

#[derive(Default)]
pub struct GatewayBuilder {
    routes: Option<RouteTable>,
    catalog: Option<Catalog>,
    pricing: Option<PricingTable>,
    ledger: Option<LedgerHandle>,
    vertex_native: Option<VertexNativeProvider>,
    timeouts: Option<StreamTimeouts>,
    default_tenant: Option<String>,
    embed_routes: Option<crate::routing::embeddings::EmbeddingRouteTable>,
    embedders:
        Option<std::collections::HashMap<String, Arc<dyn crate::embeddings::EmbeddingProvider>>>,
    embed_default_input_per_mtok: Option<f64>,
    guard: Option<GuardEngine>,
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

    /// Fire cost + ledger + metrics for a completed call (buffered side-effects).
    #[allow(clippy::too_many_arguments)]
    fn record(
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
        self.ledger.enqueue(UsageEntry {
            ts: Utc::now(),
            tenant: self.tenant_of(ctx).to_string(),
            workspace: ctx.workspace.clone(),
            user: ctx.user.clone(),
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
        });
        let span_lane = if lane == "native" {
            Lane::NativeVertex
        } else {
            Lane::Standard
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
        .emit_metrics(started.elapsed().as_secs_f64());
    }

    /// Resolve the native Vertex committed stream (shared by chat/chat_stream).
    ///
    /// NOTE: the native lane is bounded only by the reqwest client timeout
    /// (`config.request_timeout`); the first-chunk/idle `StreamTimeouts` are not
    /// applied here yet — a tracked follow-up alongside native-lane fallback.
    async fn native_committed(
        &self,
        req: &ChatRequest,
        legs: &[ChainLeg],
    ) -> Result<CommittedStream, GatewayError> {
        let leg = legs
            .iter()
            .find(|l| l.provider == "vertex")
            .ok_or_else(|| GatewayError::NativeFeatureUnsupported {
                feature: "native-vertex".into(),
                route: req.model.clone(),
            })?;
        let provider = self
            .vertex_native
            .as_ref()
            .ok_or_else(|| GatewayError::BadRequest("native vertex lane not configured".into()))?;
        provider
            .stream_generate(&leg.model, req, leg.region.as_deref())
            .await
            .map(|stream| CommittedStream::single("vertex".into(), leg.model.clone(), stream))
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
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
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
            Lane::NativeVertex => (self.native_committed(&req, &legs).await?, "native", 1),
        };
        let model = committed.model.clone();
        let guard = StreamSideEffects::new(
            self.ledger.clone(),
            self.pricing.clone(),
            req.model.clone(),
            self.tenant_of(ctx).to_string(),
            ctx.workspace.clone(),
            ctx.user.clone(),
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
    /// side-effects, return the completion.
    pub async fn chat(
        &self,
        req: ChatRequest,
        ctx: &RequestCtx,
    ) -> Result<Completion, GatewayError> {
        let started = Instant::now();
        let legs = self.resolve_legs(&req)?;
        self.guard_input(&req)?;
        let request_id = ctx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
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
                let committed = self.native_committed(&req, &legs).await?;
                (collect_committed(committed).await?, "native", 1)
            }
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
        Ok(completion)
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
                    metrics::counter!(
                        "synapse_embeddings_total",
                        "route" => alias.clone(),
                        "model" => leg.model.clone(),
                        "provider" => leg.provider.clone(),
                    )
                    .increment(1);
                    metrics::histogram!(
                        "synapse_embedding_duration_seconds",
                        "route" => alias.clone(),
                        "model" => leg.model.clone(),
                        "provider" => leg.provider.clone(),
                    )
                    .record(started.elapsed().as_secs_f64());
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
        self.ledger.enqueue(UsageEntry {
            ts: Utc::now(),
            tenant: self.tenant_of(ctx).to_string(),
            workspace: ctx.workspace.clone(),
            user: ctx.user.clone(),
            route: alias.to_string(),
            provider: leg.provider.clone(),
            model: leg.model.clone(),
            lane: "embedding".into(),
            input_tokens,
            output_tokens: 0,
            cost_usd: cost,
            request_id: ctx
                .request_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            status: "ok".into(),
            op: "embedding".into(),
        });
    }
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

    pub fn build(self) -> anyhow::Result<Gateway> {
        Ok(Gateway {
            routes: Arc::new(
                self.routes
                    .ok_or_else(|| anyhow::anyhow!("Gateway: routes required"))?,
            ),
            catalog: Arc::new(
                self.catalog
                    .ok_or_else(|| anyhow::anyhow!("Gateway: catalog required"))?,
            ),
            pricing: Arc::new(
                self.pricing
                    .ok_or_else(|| anyhow::anyhow!("Gateway: pricing required"))?,
            ),
            ledger: self
                .ledger
                .ok_or_else(|| anyhow::anyhow!("Gateway: ledger required"))?,
            vertex_native: self.vertex_native.map(Arc::new),
            timeouts: self.timeouts.unwrap_or_default(),
            default_tenant: self.default_tenant.unwrap_or_else(|| "unattributed".into()),
            embed_routes: Arc::new(self.embed_routes.unwrap_or_default()),
            embedders: self.embedders.unwrap_or_default(),
            embed_default_input_per_mtok: self.embed_default_input_per_mtok.unwrap_or(0.10),
            guard: Arc::new(self.guard.unwrap_or_else(GuardEngine::empty)),
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
    route: String,
    tenant: String,
    workspace: Option<String>,
    user: Option<String>,
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
        route: String,
        tenant: String,
        workspace: Option<String>,
        user: Option<String>,
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
            route,
            tenant,
            workspace,
            user,
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
            workspace: self.workspace.clone(),
            user: self.user.clone(),
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
            self.workspace.as_deref(),
            self.legs_attempted,
            true,
        )
        .emit_metrics(self.started.elapsed().as_secs_f64());
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
async fn collect_committed(committed: CommittedStream) -> Result<Completion, GatewayError> {
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
                "route".into(),
                "tenant".into(),
                None,
                None,
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
            "route".into(),
            "acme".into(),
            None,
            None,
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
            .build()
            .unwrap();
        let req = serde_json::from_value(serde_json::json!(
            {"model":"fast","messages":[{"role":"user","content":"hi"}]}))
        .unwrap();
        let ctx = RequestCtx {
            tenant: Some("acme".into()),
            workspace: None,
            user: Some("user-42".into()),
            request_id: Some("corr-123".into()),
        };
        let c = gw.chat(req, &ctx).await.unwrap();
        assert_eq!(c.content, "hi");
        assert_eq!(c.input_tokens, 3);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tenant, "acme");
        assert_eq!(rows[0].user.as_deref(), Some("user-42"));
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
            .build()
            .unwrap();
        let req = serde_json::from_value(serde_json::json!(
            {"model":"fast","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .unwrap();
        let mut stream = gw.chat_stream(req, &RequestCtx::default()).await.unwrap();
        let mut got = false;
        while let Some(i) = stream.next().await {
            if matches!(i.unwrap(), StreamItem::Delta(ref t) if t == "go") {
                got = true;
            }
        }
        drop(stream);
        assert!(got);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(store.entries().len(), 1);
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
        let resp = gw.embed(req, RequestCtx::default()).await.unwrap();
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
