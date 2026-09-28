//! Pluggable cost ledger. The hot path enqueues onto a bounded channel drained
//! by a background writer; on a full channel we drop + count, never block.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::telemetry::GatewayMetrics;

pub mod connect;
pub mod event;
#[cfg(feature = "ledger-postgres")]
pub mod postgres;
#[cfg(feature = "ledger-pubsub")]
pub mod pubsub;
#[cfg(feature = "ledger-sns")]
pub mod sns;
#[cfg(feature = "ledger-sqlite")]
pub mod sqlite;

#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub ts: DateTime<Utc>,
    pub tenant: String,
    pub workspace: Option<String>,
    pub user: Option<String>,
    pub thread: Option<String>,
    pub message: Option<String>,
    pub route: String,
    pub provider: String,
    pub model: String,
    pub lane: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_id: String,
    pub status: String,
    /// Lane discriminator for the ledger: "chat" or "embedding".
    pub op: String,
    /// Caller-supplied classification of the work this request serves
    /// (`x-synapse-user-task-type`). Free-form; the gateway never interprets it.
    pub user_task_type: Option<String>,
    /// Classification of the work the *gateway* performed: the caller's
    /// `x-synapse-ai-task-type` when supplied, else inferred from the route
    /// alias, else `"simple"`. Always resolved, so never optional.
    pub ai_task_type: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger backend error: {0}")]
    Backend(String),
}

#[async_trait]
pub trait LedgerStore: Send + Sync {
    async fn record(&self, entry: &UsageEntry) -> Result<(), LedgerError>;
}

/// Discards all usage events. Used when no ledger sink could be connected.
#[derive(Default)]
pub struct NoopLedger;

#[async_trait]
impl LedgerStore for NoopLedger {
    async fn record(&self, _entry: &UsageEntry) -> Result<(), LedgerError> {
        Ok(())
    }
}

/// Fire-and-forget handle. Cloneable; the hot path calls `enqueue`.
#[derive(Clone)]
pub struct LedgerHandle {
    tx: mpsc::Sender<UsageEntry>,
    metrics: Arc<GatewayMetrics>,
}

impl LedgerHandle {
    /// Spawn the background writer draining into `store`, recording no metrics.
    pub fn spawn(store: Arc<dyn LedgerStore>, capacity: usize) -> Self {
        Self::spawn_with_metrics(store, capacity, GatewayMetrics::noop())
    }

    /// Spawn the background writer draining into `store`. `capacity` bounds the
    /// channel; a full channel drops the entry and bumps `ledger_dropped_total`.
    pub fn spawn_with_metrics(
        store: Arc<dyn LedgerStore>,
        capacity: usize,
        metrics: Arc<GatewayMetrics>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<UsageEntry>(capacity);
        let writer_metrics = metrics.clone();
        tokio::spawn(async move {
            while let Some(entry) = rx.recv().await {
                if let Err(e) = store.record(&entry).await {
                    tracing::warn!(
                        error = %e,
                        tenant = %entry.tenant,
                        request_id = %entry.request_id,
                        "ledger write failed"
                    );
                    writer_metrics.ledger_error("writer");
                }
            }
            tracing::warn!("ledger background writer stopped");
        });
        Self { tx, metrics }
    }

    /// Non-blocking enqueue. Never awaits the write; drops + counts on full.
    pub fn enqueue(&self, entry: UsageEntry) {
        if self.tx.try_send(entry).is_err() {
            self.metrics.ledger_dropped();
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct InMemoryLedger {
    pub entries: Mutex<Vec<UsageEntry>>,
}

impl InMemoryLedger {
    /// Snapshot the recorded entries (test-only read accessor).
    #[cfg(test)]
    pub fn entries(&self) -> Vec<UsageEntry> {
        self.entries.lock().clone()
    }
}

#[async_trait]
impl LedgerStore for InMemoryLedger {
    async fn record(&self, entry: &UsageEntry) -> Result<(), LedgerError> {
        self.entries.lock().push(entry.clone());
        Ok(())
    }
}

/// Records each entry to every configured sink, concurrently and independently.
/// A sink failing never blocks the others; per-sink failures are logged and
/// counted on `synapse_ledger_errors_total{backend=<label>}`. Always returns
/// `Ok` — the ledger is fire-and-forget; the fan-out owns error reporting.
pub struct FanoutLedger {
    sinks: Vec<(&'static str, Arc<dyn LedgerStore>)>,
    metrics: Arc<GatewayMetrics>,
}

impl FanoutLedger {
    pub fn new(sinks: Vec<(&'static str, Arc<dyn LedgerStore>)>) -> Self {
        Self {
            sinks,
            metrics: GatewayMetrics::noop(),
        }
    }

    /// Count per-sink failures on `metrics`.
    pub fn with_metrics(self, metrics: Arc<GatewayMetrics>) -> Self {
        Self { metrics, ..self }
    }
}

#[async_trait]
impl LedgerStore for FanoutLedger {
    async fn record(&self, entry: &UsageEntry) -> Result<(), LedgerError> {
        let metrics = &self.metrics;
        let futs = self.sinks.iter().map(|(label, sink)| async move {
            if let Err(e) = sink.record(entry).await {
                tracing::warn!(backend = label, error = %e, tenant = %entry.tenant, "ledger sink write failed");
                metrics.ledger_error(label);
            }
        });
        join_all(futs).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> UsageEntry {
        UsageEntry {
            ts: Utc::now(),
            tenant: "acme".into(),
            workspace: None,
            user: None,
            thread: None,
            message: None,
            route: "fast".into(),
            provider: "vertex".into(),
            model: "gemini-3-flash".into(),
            lane: "standard".into(),
            input_tokens: 3,
            output_tokens: 5,
            cost_usd: 0.001,
            request_id: "r1".into(),
            status: "ok".into(),
            op: "chat".into(),
            user_task_type: None,
            ai_task_type: "simple".into(),
        }
    }

    #[tokio::test]
    async fn in_memory_records_directly() {
        let store = InMemoryLedger::default();
        store.record(&entry()).await.unwrap();
        assert_eq!(store.entries.lock().len(), 1);
    }

    #[tokio::test]
    async fn handle_drains_into_store() {
        let store = Arc::new(InMemoryLedger::default());
        let handle = LedgerHandle::spawn(store.clone(), 16);
        handle.enqueue(entry());
        // give the writer task a tick to drain
        for _ in 0..50 {
            if store.entries.lock().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(store.entries.lock().len(), 1);
    }

    struct FailingLedger;
    #[async_trait]
    impl LedgerStore for FailingLedger {
        async fn record(&self, _e: &UsageEntry) -> Result<(), LedgerError> {
            Err(LedgerError::Backend("boom".into()))
        }
    }

    #[tokio::test]
    async fn fanout_records_to_all_sinks() {
        let a = Arc::new(InMemoryLedger::default());
        let b = Arc::new(InMemoryLedger::default());
        let fanout = FanoutLedger::new(vec![
            ("a", a.clone() as Arc<dyn LedgerStore>),
            ("b", b.clone() as Arc<dyn LedgerStore>),
        ]);
        fanout.record(&entry()).await.unwrap();
        assert_eq!(a.entries.lock().len(), 1);
        assert_eq!(b.entries.lock().len(), 1);
    }

    #[tokio::test]
    async fn handle_keeps_accepting_after_write_failures() {
        let handle = LedgerHandle::spawn(Arc::new(FailingLedger), 16);
        handle.enqueue(entry());
        handle.enqueue(entry());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.enqueue(entry());
    }

    #[tokio::test]
    async fn fanout_survives_a_failing_sink_and_returns_ok() {
        let healthy = Arc::new(InMemoryLedger::default());
        let fanout = FanoutLedger::new(vec![
            ("fail", Arc::new(FailingLedger) as Arc<dyn LedgerStore>),
            ("mem", healthy.clone() as Arc<dyn LedgerStore>),
        ]);
        let r = fanout.record(&entry()).await;
        assert!(r.is_ok());
        assert_eq!(healthy.entries.lock().len(), 1);
    }

    #[cfg(feature = "server")]
    async fn scraped_until(exporter: &crate::telemetry::MetricsExporter, line: &str) -> String {
        for _ in 0..50 {
            let text = crate::telemetry::scrape(exporter);
            if text.lines().any(|l| l == line) {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "missing `{line}` in:\n{}",
            crate::telemetry::scrape(exporter)
        );
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn writer_failures_are_counted_on_the_given_metrics() {
        let (m, exporter) = crate::telemetry::test_metrics();
        let handle = LedgerHandle::spawn_with_metrics(Arc::new(FailingLedger), 16, m);
        handle.enqueue(entry());
        scraped_until(
            &exporter,
            r#"synapse_ledger_errors_total{backend="writer"} 1"#,
        )
        .await;
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn full_channel_drops_are_counted_on_the_given_metrics() {
        struct SlowLedger;
        #[async_trait]
        impl LedgerStore for SlowLedger {
            async fn record(&self, _e: &UsageEntry) -> Result<(), LedgerError> {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(())
            }
        }
        let (m, exporter) = crate::telemetry::test_metrics();
        let handle = LedgerHandle::spawn_with_metrics(Arc::new(SlowLedger), 1, m);
        (0..3).for_each(|_| handle.enqueue(entry()));
        let dropped: u64 = crate::telemetry::scrape(&exporter)
            .lines()
            .find_map(|l| l.strip_prefix("synapse_ledger_dropped_total "))
            .and_then(|v| v.parse().ok())
            .unwrap();
        assert!(dropped >= 1, "expected at least one drop, got {dropped}");
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn fanout_sink_failures_are_counted_per_backend() {
        let (m, exporter) = crate::telemetry::test_metrics();
        let fanout = FanoutLedger::new(vec![
            ("fail", Arc::new(FailingLedger) as Arc<dyn LedgerStore>),
            (
                "mem",
                Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>,
            ),
        ])
        .with_metrics(m);
        fanout.record(&entry()).await.unwrap();
        scraped_until(
            &exporter,
            r#"synapse_ledger_errors_total{backend="fail"} 1"#,
        )
        .await;
    }
}
