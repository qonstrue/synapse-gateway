//! AWS SNS ledger backend (feature `ledger-sns`). Publishes the talos-aligned
//! `UsageEvent` JSON with matching message attributes.

use async_trait::async_trait;
use aws_sdk_sns::types::MessageAttributeValue;
use aws_sdk_sns::Client;

use crate::ledger::event::UsageEvent;
use crate::ledger::{LedgerError, LedgerStore, UsageEntry};

pub struct SnsLedger {
    client: Client,
    topic_arn: String,
}

impl SnsLedger {
    /// Build an SNS client via the standard AWS credential chain. `region`
    /// overrides the default provider chain when set.
    pub async fn connect(topic_arn: &str, region: Option<&str>) -> Result<Self, LedgerError> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(r) = region {
            loader = loader.region(aws_config::Region::new(r.to_string()));
        }
        let conf = loader.load().await;
        Ok(Self {
            client: Client::new(&conf),
            topic_arn: topic_arn.to_string(),
        })
    }
}

#[async_trait]
impl LedgerStore for SnsLedger {
    async fn record(&self, e: &UsageEntry) -> Result<(), LedgerError> {
        let event = UsageEvent::from(e);
        let body =
            serde_json::to_string(&event).map_err(|e| LedgerError::Backend(e.to_string()))?;
        let mut req = self
            .client
            .publish()
            .topic_arn(&self.topic_arn)
            .message(body);
        for (k, v) in event.attributes() {
            let attr = MessageAttributeValue::builder()
                .data_type("String")
                .string_value(v)
                .build()
                .map_err(|e| LedgerError::Backend(e.to_string()))?;
            req = req.message_attributes(k, attr);
        }
        req.send()
            .await
            .map_err(|e| LedgerError::Backend(format!("sns publish: {e}")))?;
        Ok(())
    }
}
