//! GCP Pub/Sub ledger backend (feature `ledger-pubsub`). Mirrors the publisher
//! mechanics of `talos-core/src/tool_events.rs`.

use std::collections::HashMap;

use async_trait::async_trait;
use google_cloud_pubsub::client::{Client, ClientConfig};
use google_cloud_pubsub::topic::Topic;

use crate::ledger::event::UsageEvent;
use crate::ledger::{LedgerError, LedgerStore, UsageEntry};

pub struct PubsubLedger {
    topic: Topic,
}

impl PubsubLedger {
    /// Authenticate via ADC and resolve the topic.
    pub async fn connect(project: &str, topic_id: &str) -> Result<Self, LedgerError> {
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
        Ok(Self {
            topic: client.topic(topic_id),
        })
    }
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
