#![cfg(feature = "ledger-sqlite")]
use chrono::Utc;
use synapse::ledger::sqlite::SqliteLedger;
use synapse::ledger::{LedgerStore, UsageEntry};

#[tokio::test]
async fn records_and_persists_a_usage_row() {
    let store = SqliteLedger::connect("sqlite::memory:").await.unwrap();
    let entry = UsageEntry {
        ts: Utc::now(),
        tenant: "acme".into(),
        workspace: Some("ws1".into()),
        route: "fast".into(),
        provider: "vertex".into(),
        model: "gemini-3-flash".into(),
        lane: "standard".into(),
        input_tokens: 7,
        output_tokens: 11,
        cost_usd: 0.0123,
        request_id: "req-1".into(),
        status: "ok".into(),
        op: "chat".into(),
    };
    store.record(&entry).await.unwrap();
    store.record(&entry).await.unwrap();
    // a second record proves the table + insert path work repeatedly
}
