//! GCP Pub/Sub ledger backend (feature `ledger-pubsub`). Mirrors the publisher
//! mechanics of `talos-core/src/tool_events.rs`.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use google_cloud_pubsub::client::{Client, ClientConfig};
use google_cloud_pubsub::topic::Topic;

use crate::ledger::event::UsageEvent;
use crate::ledger::{LedgerError, LedgerStore, UsageEntry};

const PUBSUB_CONNECT_MAX_ATTEMPTS: usize = 8;
const PUBSUB_CONNECT_MIN_DELAY: Duration = Duration::from_millis(250);
const PUBSUB_CONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

pub struct PubsubLedger {
    topic: Topic,
}

impl PubsubLedger {
    /// Authenticate via ADC and resolve the topic. Retries transient gRPC/transport
    /// failures (common when metadata or egress is not ready on a fresh node).
    pub async fn connect(project: &str, topic_id: &str) -> Result<Self, LedgerError> {
        let project = project.to_string();
        let topic_id = topic_id.to_string();
        let builder = ExponentialBuilder::default()
            .with_max_times(PUBSUB_CONNECT_MAX_ATTEMPTS.saturating_sub(1))
            .with_min_delay(PUBSUB_CONNECT_MIN_DELAY)
            .with_max_delay(PUBSUB_CONNECT_MAX_DELAY)
            .with_factor(2.0)
            .with_jitter();

        (|| connect_once(&project, &topic_id))
            .retry(builder)
            .when(is_retryable_pubsub_connect_error)
            .notify(|err, dur| {
                tracing::warn!(
                    project = %project,
                    topic = %topic_id,
                    delay_ms = dur.as_millis() as u64,
                    error = %err,
                    "retrying pubsub ledger connect"
                );
            })
            .await
    }
}

async fn connect_once(project: &str, topic_id: &str) -> Result<PubsubLedger, LedgerError> {
    // `with_project_id` does not exist in 0.30; set the field after auth
    // (exactly as talos does).
    let mut config = ClientConfig::default()
        .with_auth()
        .await
        .map_err(|e| LedgerError::Backend(format!("pubsub auth: {e}")))?;
    config.project_id = Some(project.to_string());
    let client = Client::new(config)
        .await
        .map_err(|e| LedgerError::Backend(format!("pubsub client: {e}")))?;
    Ok(PubsubLedger {
        topic: client.topic(topic_id),
    })
}

/// Classify connect-time Pub/Sub errors. Permission failures are not retried.
pub(crate) fn is_retryable_pubsub_connect_error(err: &LedgerError) -> bool {
    let LedgerError::Backend(message) = err;
    let m = message.to_ascii_lowercase();
    if m.contains("permission_denied")
        || m.contains("not authorized")
        || m.contains("unauthenticated")
        || m.contains("invalid_grant")
        || m.contains("topic not found")
        || m.contains("not found")
            && m.contains("topic")
    {
        return false;
    }
    m.contains("transport error")
        || m.contains("tonic")
        || m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("connect")
        || m.contains("timeout")
        || m.contains("timed out")
        || m.contains("unavailable")
        || m.contains("dns")
        || m.contains("temporarily unavailable")
        || m.contains("network")
}

#[async_trait]
impl LedgerStore for PubsubLedger {
    async fn record(&self, e: &UsageEntry) -> Result<(), LedgerError> {
        use google_cloud_googleapis::pubsub::v1::PubsubMessage;

        let event = UsageEvent::from(e);
        let data = serde_json::to_vec(&event).map_err(|e| LedgerError::Backend(e.to_string()))?;
        let attributes: HashMap<String, String> = event
            .attributes()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let msg = PubsubMessage {
            data,
            attributes,
            ordering_key: event.request_id.clone(),
            ..Default::default()
        };
        let publisher = self.topic.new_publisher(None);
        publisher
            .publish_immediately(vec![msg], None)
            .await
            .map_err(|e| LedgerError::Backend(format!("pubsub publish: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(msg: &str) -> LedgerError {
        LedgerError::Backend(msg.into())
    }

    #[test]
    fn transport_errors_are_retryable() {
        assert!(is_retryable_pubsub_connect_error(&backend(
            "pubsub client: tonic error : transport error"
        )));
        assert!(is_retryable_pubsub_connect_error(&backend(
            "pubsub auth: connection refused"
        )));
    }

    #[test]
    fn permission_errors_are_not_retryable() {
        assert!(!is_retryable_pubsub_connect_error(&backend(
            "pubsub client: status: PermissionDenied, message: User not authorized"
        )));
        assert!(!is_retryable_pubsub_connect_error(&backend(
            "pubsub client: topic not found"
        )));
    }
}
