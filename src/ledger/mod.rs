//! Pluggable cost ledger. The hot path enqueues onto a bounded channel drained
//! by a background writer; on a full channel we drop + count, never block.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use metrics::counter;
use parking_lot::Mutex;
use tokio::sync::mpsc;

#[cfg(feature = "ledger-sqlite")]
pub mod sqlite;
#[cfg(feature = "ledger-postgres")]
pub mod postgres;

#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub ts: DateTime<Utc>,
    pub tenant: String,
    pub workspace: Option<String>,
    pub route: String,
    pub provider: String,
    pub model: String,
    pub lane: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_id: String,
    pub status: String,
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

/// Fire-and-forget handle. Cloneable; the hot path calls `enqueue`.
#[derive(Clone)]
pub struct LedgerHandle {
    tx: mpsc::Sender<UsageEntry>,
}

impl LedgerHandle {
    /// Spawn the background writer draining into `store`. `capacity` bounds the
    /// channel; a full channel drops the entry and bumps `ledger_dropped_total`.
    pub fn spawn(store: Arc<dyn LedgerStore>, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<UsageEntry>(capacity);
        tokio::spawn(async move {
            while let Some(entry) = rx.recv().await {
                if let Err(e) = store.record(&entry).await {
                    tracing::warn!(error = %e, tenant = %entry.tenant, "ledger write failed");
                    counter!("synapse_ledger_errors_total").increment(1);
                }
            }
        });
        Self { tx }
    }

    /// Non-blocking enqueue. Never awaits the write; drops + counts on full.
    pub fn enqueue(&self, entry: UsageEntry) {
        if self.tx.try_send(entry).is_err() {
            counter!("synapse_ledger_dropped_total").increment(1);
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct InMemoryLedger {
    pub entries: Mutex<Vec<UsageEntry>>,
}

#[async_trait]
impl LedgerStore for InMemoryLedger {
    async fn record(&self, entry: &UsageEntry) -> Result<(), LedgerError> {
        self.entries.lock().push(entry.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> UsageEntry {
        UsageEntry {
            ts: Utc::now(), tenant: "acme".into(), workspace: None, route: "fast".into(),
            provider: "vertex".into(), model: "gemini-3-flash".into(), lane: "standard".into(),
            input_tokens: 3, output_tokens: 5, cost_usd: 0.001, request_id: "r1".into(), status: "ok".into(),
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
            if store.entries.lock().len() == 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(store.entries.lock().len(), 1);
    }
}
