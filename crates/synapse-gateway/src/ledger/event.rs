//! talos-aligned published usage event (camelCase JSON). Decouples the wire
//! format from the DB-shaped `UsageEntry`. Mirrors the conventions of
//! `talos-core/src/tool_events.rs` (`ToolEvent`): `rename_all = "camelCase"`,
//! tenancy as `namespace`, a `type` discriminator, RFC3339 timestamp.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::ledger::UsageEntry;

/// Marketplace ledger subscription discriminator (`attributes.EventType`).
pub const LEDGER_EVENT_TYPE: &str = "Ledger.LLMTokensConsumed";

/// One terminal usage event, published to a topic per request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEvent {
    /// Tenancy. talos calls this `namespace`; synapse maps its `tenant` onto it.
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// End-user attribution within the namespace (`x-synapse-user`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub route: String,
    pub provider: String,
    pub model: String,
    pub lane: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub status: String,
    /// Lane discriminator: "chat" or "embedding".
    pub op: String,
}

impl From<&UsageEntry> for UsageEvent {
    fn from(e: &UsageEntry) -> Self {
        Self {
            namespace: e.tenant.clone(),
            workspace: e.workspace.clone(),
            user: e.user.clone(),
            request_id: e.request_id.clone(),
            timestamp: e.ts,
            event_type: "usage",
            route: e.route.clone(),
            provider: e.provider.clone(),
            model: e.model.clone(),
            lane: e.lane.clone(),
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            cost_usd: e.cost_usd,
            status: e.status.clone(),
            op: e.op.clone(),
        }
    }
}

impl UsageEvent {
    /// Broker message attributes for subscription filtering (talos-aligned keys
    /// plus synapse-useful `provider`/`status`). Returned as ordered pairs so
    /// both backends can convert to their SDK-specific attribute type.
    pub fn attributes(&self) -> Vec<(&'static str, String)> {
        vec![
            ("EventType", LEDGER_EVENT_TYPE.to_string()),
            ("namespace", self.namespace.clone()),
            ("requestId", self.request_id.clone()),
            ("type", self.event_type.to_string()),
            ("provider", self.provider.clone()),
            ("status", self.status.clone()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry() -> UsageEntry {
        UsageEntry {
            ts: Utc.with_ymd_and_hms(2026, 6, 10, 15, 30, 45).unwrap(),
            tenant: "acme".into(),
            workspace: None,
            user: None,
            route: "gemini-pro".into(),
            provider: "vertex".into(),
            model: "gemini-3-pro".into(),
            lane: "standard".into(),
            input_tokens: 3,
            output_tokens: 5,
            cost_usd: 0.001,
            request_id: "req-1".into(),
            status: "ok".into(),
            op: "chat".into(),
        }
    }

    #[test]
    fn serializes_talos_aligned_camelcase() {
        let v = serde_json::to_value(UsageEvent::from(&entry())).unwrap();
        assert_eq!(v["namespace"], "acme");
        assert_eq!(v["type"], "usage");
        assert_eq!(v["requestId"], "req-1");
        assert_eq!(v["inputTokens"], 3);
        assert_eq!(v["outputTokens"], 5);
        assert_eq!(v["costUsd"], 0.001);
        assert_eq!(v["lane"], "standard");
        assert_eq!(v["op"], "chat");
        assert!(v.get("workspace").is_none());
        assert!(v.get("user").is_none());
        assert!(v.get("request_id").is_none());
        assert!(v.get("input_tokens").is_none());
    }

    #[test]
    fn serializes_user_when_present() {
        let mut e = entry();
        e.user = Some("user-42".into());
        let v = serde_json::to_value(UsageEvent::from(&e)).unwrap();
        assert_eq!(v["user"], "user-42");
    }

    #[test]
    fn serializes_embedding_op() {
        let mut e = entry();
        e.op = "embedding".into();
        let v = serde_json::to_value(UsageEvent::from(&e)).unwrap();
        assert_eq!(v["op"], "embedding");
    }

    #[test]
    fn attributes_have_talos_keys() {
        let attrs = UsageEvent::from(&entry()).attributes();
        let keys: Vec<&str> = attrs.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                "EventType",
                "namespace",
                "requestId",
                "type",
                "provider",
                "status"
            ]
        );
        assert_eq!(attrs[0].1, LEDGER_EVENT_TYPE);
        assert_eq!(attrs[1].1, "acme");
    }
}
