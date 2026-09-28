//! OpenTelemetry metrics. Every instrument the gateway emits lives on one
//! [`GatewayMetrics`], passed explicitly (there is no global recorder). The
//! Prometheus and OTLP exporters sit behind the `server` feature.

use std::sync::Arc;

use opentelemetry::metrics::{
    Counter, Gauge, Histogram, Meter, MeterProvider as _, NoopMeterProvider,
};
use opentelemetry::KeyValue;

use crate::observability::GenAiSpan;
use crate::routing::classify::Lane;

#[cfg(feature = "server")]
pub use exporter::{install, metrics_router, scrape, test_metrics, MetricsExporter};

/// Second-scaled histogram boundaries; OTel's defaults are millisecond-scaled.
pub const SECONDS_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// A duration histogram in seconds with [`SECONDS_BUCKETS`] boundaries.
pub fn seconds_histogram(meter: &Meter, name: &'static str) -> Histogram<f64> {
    meter
        .f64_histogram(name)
        .with_boundaries(SECONDS_BUCKETS.to_vec())
        .build()
}

/// Every instrument the gateway records. Build once, share behind an `Arc`.
pub struct GatewayMetrics {
    requests: Counter<u64>,
    request_duration: Histogram<f64>,
    input_tokens: Counter<u64>,
    output_tokens: Counter<u64>,
    embeddings: Counter<u64>,
    embedding_duration: Histogram<f64>,
    passthrough: Counter<u64>,
    passthrough_fallback: Counter<u64>,
    jev_extraction: Counter<u64>,
    ledger_errors: Counter<u64>,
    ledger_dropped: Counter<u64>,
    retry_attempts: Counter<u64>,
    resilience_calls: Counter<u64>,
    resilience_call_duration: Histogram<f64>,
    breaker_transitions: Counter<u64>,
    breaker_state: Gauge<f64>,
    guard_scans: Counter<u64>,
    guard_matches: Counter<u64>,
    guard_scan_duration: Histogram<f64>,
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self::new(&NoopMeterProvider::new().meter("synapse-gateway"))
    }
}

impl GatewayMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter.u64_counter("synapse_requests_total").build(),
            request_duration: seconds_histogram(meter, "synapse_request_duration_seconds"),
            input_tokens: meter.u64_counter("synapse_input_tokens_total").build(),
            output_tokens: meter.u64_counter("synapse_output_tokens_total").build(),
            embeddings: meter.u64_counter("synapse_embeddings_total").build(),
            embedding_duration: seconds_histogram(meter, "synapse_embedding_duration_seconds"),
            passthrough: meter.u64_counter("synapse_passthrough_total").build(),
            passthrough_fallback: meter
                .u64_counter("synapse_passthrough_fallback_total")
                .build(),
            jev_extraction: meter.u64_counter("synapse_jev_extraction_total").build(),
            ledger_errors: meter.u64_counter("synapse_ledger_errors_total").build(),
            ledger_dropped: meter.u64_counter("synapse_ledger_dropped_total").build(),
            retry_attempts: meter
                .u64_counter("synapse_resilience_retry_attempts_total")
                .build(),
            resilience_calls: meter.u64_counter("synapse_resilience_calls_total").build(),
            resilience_call_duration: seconds_histogram(
                meter,
                "synapse_resilience_call_duration_seconds",
            ),
            breaker_transitions: meter
                .u64_counter("synapse_resilience_breaker_transitions_total")
                .build(),
            breaker_state: meter.f64_gauge("synapse_resilience_breaker_state").build(),
            guard_scans: meter.u64_counter("synapse_guard_scans_total").build(),
            guard_matches: meter.u64_counter("synapse_guard_matches_total").build(),
            guard_scan_duration: seconds_histogram(meter, "synapse_guard_scan_duration_seconds"),
        }
    }

    /// Instruments that record nothing, for embedders and tests that never
    /// read metrics.
    pub fn noop() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One served chat request: count, latency, and token totals.
    pub fn request(&self, span: &GenAiSpan, latency_secs: f64) {
        let labels = [
            KeyValue::new("route", span.route.clone()),
            KeyValue::new("model", span.response_model.clone()),
            KeyValue::new("system", span.system),
            KeyValue::new("lane", lane_label(&span.lane)),
        ];
        self.requests.add(1, &labels);
        self.request_duration.record(latency_secs, &labels);
        self.input_tokens.add(span.input_tokens, &labels);
        self.output_tokens.add(span.output_tokens, &labels);
    }

    pub fn embedding(&self, route: &str, model: &str, provider: &str, secs: f64) {
        let labels = [
            KeyValue::new("route", route.to_string()),
            KeyValue::new("model", model.to_string()),
            KeyValue::new("provider", provider.to_string()),
        ];
        self.embeddings.add(1, &labels);
        self.embedding_duration.record(secs, &labels);
    }

    pub fn passthrough(&self, provider: &'static str, model: &str, action: &str, ok: bool) {
        self.passthrough.add(
            1,
            &[
                KeyValue::new("provider", provider),
                KeyValue::new("model", model.to_string()),
                KeyValue::new("action", action.to_string()),
                KeyValue::new("status", if ok { "ok" } else { "error" }),
            ],
        );
    }

    pub fn passthrough_fallback(&self, from_model: &str, to_model: &str) {
        self.passthrough_fallback.add(
            1,
            &[
                KeyValue::new("from_model", from_model.to_string()),
                KeyValue::new("to_model", to_model.to_string()),
            ],
        );
    }

    pub fn jev_extraction(&self, route: &str, degraded: bool) {
        self.jev_extraction.add(
            1,
            &[
                KeyValue::new("route", route.to_string()),
                KeyValue::new("degraded", degraded.to_string()),
            ],
        );
    }

    pub fn ledger_error(&self, backend: &'static str) {
        self.ledger_errors
            .add(1, &[KeyValue::new("backend", backend)]);
    }

    pub fn ledger_dropped(&self) {
        self.ledger_dropped.add(1, &[]);
    }

    pub fn retry_attempt(&self, label: &'static str) {
        self.retry_attempts.add(1, &[KeyValue::new("label", label)]);
    }

    pub fn resilience_call(&self, label: &'static str, outcome: &'static str, secs: f64) {
        let labels = [
            KeyValue::new("label", label),
            KeyValue::new("outcome", outcome),
        ];
        self.resilience_calls.add(1, &labels);
        self.resilience_call_duration.record(secs, &labels);
    }

    /// A breaker transition plus its new state (0 closed, 1 open, 2 half-open).
    pub fn breaker_transition(&self, name: &'static str, transition: &'static str, state: u8) {
        self.breaker_transitions.add(
            1,
            &[
                KeyValue::new("name", name),
                KeyValue::new("transition", transition),
            ],
        );
        self.breaker_state
            .record(f64::from(state), &[KeyValue::new("name", name)]);
    }

    pub fn guard_scan(&self, policy: &str, outcome: &'static str, secs: f64) {
        self.guard_scans.add(
            1,
            &[
                KeyValue::new("policy", policy.to_string()),
                KeyValue::new("outcome", outcome),
            ],
        );
        self.guard_scan_duration
            .record(secs, &[KeyValue::new("policy", policy.to_string())]);
    }

    pub fn guard_match(&self, policy: &str, scanner: &str, severity: &'static str) {
        self.guard_matches.add(
            1,
            &[
                KeyValue::new("policy", policy.to_string()),
                KeyValue::new("scanner", scanner.to_string()),
                KeyValue::new("severity", severity),
            ],
        );
    }
}

fn lane_label(lane: &Lane) -> &'static str {
    match lane {
        Lane::Standard => "standard",
        Lane::NativeVertex => "native",
        Lane::Jev => "jev",
    }
}

#[cfg(feature = "server")]
mod exporter {
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::Resource;
    use prometheus::{Registry, TextEncoder, TEXT_FORMAT};
    use tap::Pipe;

    use super::GatewayMetrics;

    /// The Prometheus registry plus the provider feeding it. Keep it alive for
    /// the process lifetime: dropping the last provider handle shuts every
    /// reader down, and scrapes come back empty.
    #[derive(Clone, Debug)]
    pub struct MetricsExporter {
        pub registry: Registry,
        _provider: SdkMeterProvider,
    }

    /// Build the meter provider and the gateway's instruments. Prometheus is
    /// always on; OTLP/HTTP is added when `otlp_endpoint` (a base collector URL
    /// such as `http://collector:4318`) is set. opentelemetry-otlp 0.32 uses
    /// programmatic endpoints verbatim, so `/v1/metrics` is appended here.
    pub fn install(
        otlp_endpoint: Option<&str>,
        service_name: &str,
    ) -> anyhow::Result<(Arc<GatewayMetrics>, MetricsExporter)> {
        let registry = Registry::new();
        let prometheus = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .without_scope_info()
            .without_target_info()
            .without_counter_suffixes()
            .without_units()
            .build()?;
        let otlp = otlp_endpoint
            .map(|endpoint| {
                opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_endpoint(format!("{}/v1/metrics", endpoint.trim_end_matches('/')))
                    .build()
            })
            .transpose()?;
        let provider = SdkMeterProvider::builder()
            .with_reader(prometheus)
            .with_resource(
                Resource::builder()
                    .with_service_name(service_name.to_string())
                    .build(),
            )
            .pipe(|builder| match otlp {
                Some(exporter) => builder.with_reader(PeriodicReader::builder(exporter).build()),
                None => builder,
            })
            .build();
        let metrics = Arc::new(GatewayMetrics::new(&provider.meter("synapse-gateway")));
        Ok((
            metrics,
            MetricsExporter {
                registry,
                _provider: provider,
            },
        ))
    }

    /// A private Prometheus-only exporter for tests.
    #[doc(hidden)]
    pub fn test_metrics() -> (Arc<GatewayMetrics>, MetricsExporter) {
        install(None, "synapse-gateway-test").expect("prometheus exporter builds")
    }

    /// Prometheus text exposition of everything recorded so far.
    #[doc(hidden)]
    pub fn scrape(exporter: &MetricsExporter) -> String {
        encode(exporter).unwrap_or_default()
    }

    fn encode(exporter: &MetricsExporter) -> prometheus::Result<String> {
        TextEncoder::new().encode_to_string(&exporter.registry.gather())
    }

    /// `GET /metrics` (and `GET /`) in Prometheus text format.
    pub fn metrics_router(exporter: MetricsExporter) -> Router {
        Router::new()
            .route("/metrics", get(serve))
            .route("/", get(serve))
            .with_state(exporter)
    }

    async fn serve(State(exporter): State<MetricsExporter>) -> Response {
        match encode(&exporter) {
            Ok(text) => ([(header::CONTENT_TYPE, TEXT_FORMAT)], text).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "prometheus metrics encoding failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::GenAiSpan;
    use crate::routing::classify::Lane;
    use crate::routing::executor::Completion;
    use crate::routing::stream::FinishReason;

    fn span() -> GenAiSpan {
        let c = Completion {
            provider: "qwen".into(),
            model: "qwen-max".into(),
            content: String::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            input_tokens: 3,
            output_tokens: 5,
        };
        GenAiSpan::from_completion(&c, Lane::Standard, "fast", "acme", None, 1, false)
    }

    #[test]
    fn noop_accepts_every_measurement() {
        let m = GatewayMetrics::noop();
        m.request(&span(), 0.1);
        m.embedding("embed", "text-embedding-3-small", "openai", 0.05);
        m.passthrough("vertex", "gemini-2.5-flash", "countTokens", true);
        m.passthrough_fallback("gemini-2.5-pro", "gemini-2.5-flash");
        m.jev_extraction("extract", false);
        m.ledger_error("writer");
        m.ledger_dropped();
        m.retry_attempt("qwen");
        m.resilience_call("qwen", "success", 0.3);
        m.breaker_transition("qwen", "open", 1);
        m.guard_scan("strict", "block", 0.001);
        m.guard_match("strict", "ban_substrings", "block");
    }

    #[cfg(feature = "server")]
    #[test]
    fn every_instrument_exports_with_its_labels() {
        let (m, exporter) = test_metrics();
        m.request(&span(), 0.2);
        m.embedding("embed", "text-embedding-3-small", "openai", 0.05);
        m.passthrough("vertex", "gemini-2.5-flash", "countTokens", true);
        m.passthrough_fallback("gemini-2.5-pro", "gemini-2.5-flash");
        m.jev_extraction("extract", false);
        m.ledger_error("writer");
        m.ledger_dropped();
        m.retry_attempt("qwen");
        m.resilience_call("qwen", "success", 0.3);
        m.breaker_transition("qwen", "open", 1);
        m.guard_scan("strict", "block", 0.001);
        m.guard_match("strict", "ban_substrings", "block");
        let text = scrape(&exporter);
        for line in [
            r#"synapse_requests_total{lane="standard",model="qwen-max",route="fast",system="dashscope"} 1"#,
            r#"synapse_input_tokens_total{lane="standard",model="qwen-max",route="fast",system="dashscope"} 3"#,
            r#"synapse_output_tokens_total{lane="standard",model="qwen-max",route="fast",system="dashscope"} 5"#,
            r#"synapse_request_duration_seconds_count{lane="standard",model="qwen-max",route="fast",system="dashscope"} 1"#,
            r#"synapse_embeddings_total{model="text-embedding-3-small",provider="openai",route="embed"} 1"#,
            r#"synapse_embedding_duration_seconds_count{model="text-embedding-3-small",provider="openai",route="embed"} 1"#,
            r#"synapse_passthrough_total{action="countTokens",model="gemini-2.5-flash",provider="vertex",status="ok"} 1"#,
            r#"synapse_passthrough_fallback_total{from_model="gemini-2.5-pro",to_model="gemini-2.5-flash"} 1"#,
            r#"synapse_jev_extraction_total{degraded="false",route="extract"} 1"#,
            r#"synapse_ledger_errors_total{backend="writer"} 1"#,
            "synapse_ledger_dropped_total 1",
            r#"synapse_resilience_retry_attempts_total{label="qwen"} 1"#,
            r#"synapse_resilience_calls_total{label="qwen",outcome="success"} 1"#,
            r#"synapse_resilience_call_duration_seconds_count{label="qwen",outcome="success"} 1"#,
            r#"synapse_resilience_breaker_transitions_total{name="qwen",transition="open"} 1"#,
            r#"synapse_resilience_breaker_state{name="qwen"} 1"#,
            r#"synapse_guard_scans_total{outcome="block",policy="strict"} 1"#,
            r#"synapse_guard_matches_total{policy="strict",scanner="ban_substrings",severity="block"} 1"#,
            r#"synapse_guard_scan_duration_seconds_count{policy="strict"} 1"#,
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing `{line}` in:\n{text}"
            );
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn failed_passthrough_and_latest_breaker_state_export() {
        let (m, exporter) = test_metrics();
        m.passthrough("vertex", "gemini-2.5-flash", "countTokens", false);
        m.breaker_transition("qwen", "open", 1);
        m.breaker_transition("qwen", "half_open", 2);
        let text = scrape(&exporter);
        for line in [
            r#"synapse_passthrough_total{action="countTokens",model="gemini-2.5-flash",provider="vertex",status="error"} 1"#,
            r#"synapse_resilience_breaker_state{name="qwen"} 2"#,
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing `{line}` in:\n{text}"
            );
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn exposition_has_seconds_buckets_and_no_otel_artifacts() {
        let (m, exporter) = test_metrics();
        m.request(&span(), 0.2);
        let text = scrape(&exporter);
        assert!(text.contains(
            r#"synapse_request_duration_seconds_bucket{lane="standard",model="qwen-max",route="fast",system="dashscope",le="0.25"} 1"#
        ));
        assert!(!text.contains("otel_scope"));
        assert!(!text.contains("target_info"));
        assert!(!text.contains("_total_total"));
    }

    /// Accept one OTLP request, answer 200, and return its request line.
    #[cfg(feature = "server")]
    fn serve_one_otlp_request(listener: std::net::TcpListener) -> std::io::Result<String> {
        use std::io::{BufRead, BufReader, Read, Write};

        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let head: Vec<String> = std::iter::from_fn(|| {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(n) if n > 0 && line != "\r\n" => Some(line),
                _ => None,
            }
        })
        .collect();
        let body_len = head
            .iter()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        reader.read_exact(&mut vec![0; body_len])?;
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")?;
        Ok(head.into_iter().next().unwrap_or_default())
    }

    /// Install with a local collector, check Prometheus still scrapes, then
    /// drop everything so shutdown flushes one OTLP export to the collector.
    #[cfg(feature = "server")]
    fn assert_otlp_exports_alongside_prometheus() {
        use std::sync::mpsc;
        use std::time::Duration;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(serve_one_otlp_request(listener)));

        let (m, exporter) = install(Some(&format!("http://{addr}/")), "synapse-gateway").unwrap();
        m.ledger_dropped();
        assert!(scrape(&exporter).contains("synapse_ledger_dropped_total 1"));
        drop(m);
        drop(exporter);

        let request_line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("collector received no OTLP export")
            .unwrap();
        assert!(
            request_line.starts_with("POST /v1/metrics"),
            "unexpected request line: {request_line}"
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn otlp_and_prometheus_readers_coexist() {
        assert_otlp_exports_alongside_prometheus();
    }

    #[cfg(feature = "server")]
    #[tokio::test(flavor = "multi_thread")]
    async fn otlp_exports_when_installed_inside_a_tokio_runtime() {
        assert_otlp_exports_alongside_prometheus();
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn metrics_router_serves_prometheus_text_on_metrics_and_root() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (m, exporter) = test_metrics();
        m.ledger_dropped();
        for uri in ["/metrics", "/"] {
            let resp = metrics_router(exporter.clone())
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-type"], "text/plain; version=0.0.4");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("synapse_ledger_dropped_total 1"));
        }
    }
}
