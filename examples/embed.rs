//! Embed synapse's Gateway in-process: build it programmatically and call chat().
//! Run: `cargo run --example embed` (works with `--no-default-features` too — the
//! Gateway core needs no axum/HTTP server).
use std::collections::HashMap;
use std::sync::Arc;

use synapse::gateway::{Gateway, RequestCtx};
use synapse::ledger::{FanoutLedger, InMemoryLedger, LedgerHandle, LedgerStore};
use synapse::pricing::PricingTable;
use synapse::providers::Catalog;
use synapse::routing::request::ChatRequest;
use synapse::routing::table::RouteTable;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let routes = RouteTable::from_toml_str(
        "[routes.\"fast\"]\nlegs = [{ provider = \"qwen\", model = \"qwen-max\" }]",
    )?;
    // A real deployment builds the catalog from provider creds (here: a demo key
    // against the public DashScope endpoint — the call will fail auth, which the
    // example handles gracefully).
    let env = HashMap::from([
        ("DASHSCOPE_API_KEY".to_string(), "demo".to_string()),
        (
            "DASHSCOPE_BASE_URL".to_string(),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".to_string(),
        ),
    ]);
    let catalog = Catalog::build(
        &env,
        &routes.referenced_providers(),
        std::time::Duration::from_secs(30),
    )?;

    // Fan out usage events to an in-memory ledger (swap for Postgres/Pub-Sub/SNS).
    let store: Arc<dyn LedgerStore> = Arc::new(FanoutLedger::new(vec![(
        "memory",
        Arc::new(InMemoryLedger::default()) as Arc<dyn LedgerStore>,
    )]));
    let ledger = LedgerHandle::spawn(store, 1024);

    let gateway = Gateway::builder()
        .routes(routes)
        .catalog(catalog)
        .pricing(PricingTable::default())
        .ledger(ledger)
        .default_tenant("embedded")
        .build()?;

    let req: ChatRequest = serde_json::from_value(serde_json::json!({
        "model": "fast",
        "messages": [{ "role": "user", "content": "Say hi in one word." }],
    }))?;

    let ctx = RequestCtx {
        tenant: Some("acme".into()),
        ..Default::default()
    };
    match gateway.chat(req, &ctx).await {
        Ok(c) => println!(
            "completion: {} ({}+{} tokens)",
            c.content, c.input_tokens, c.output_tokens
        ),
        Err(e) => println!("gateway error (expected without real creds): {e}"),
    }
    Ok(())
}
