//! Resilient ledger wiring: connect configured sinks independently; on failure
//! log and continue. When no sink connects, fall back to a no-op store so the
//! gateway still starts and serves traffic.

use std::sync::Arc;

use crate::config::{Config, LedgerBackend};
use crate::ledger::{FanoutLedger, LedgerError, LedgerStore, NoopLedger};

async fn try_connect_backend(
    backend: LedgerBackend,
    config: &Config,
) -> Option<Arc<dyn LedgerStore>> {
    let label = backend.label();
    let result: Result<Arc<dyn LedgerStore>, LedgerError> = match backend {
        LedgerBackend::Sqlite => {
            #[cfg(feature = "ledger-sqlite")]
            {
                let dsn = config
                    .env
                    .get("SYNAPSE_LEDGER_SQLITE_DSN")
                    .or_else(|| config.env.get("SYNAPSE_LEDGER_DSN"))
                    .cloned()
                    .unwrap_or_else(|| "sqlite://synapse.db?mode=rwc".into());
                super::sqlite::SqliteLedger::connect(&dsn)
                    .await
                    .map(|s| Arc::new(s) as Arc<dyn LedgerStore>)
            }
            #[cfg(not(feature = "ledger-sqlite"))]
            {
                Err(LedgerError::Backend(
                    "ledger-sqlite feature not enabled".into(),
                ))
            }
        }
        LedgerBackend::Postgres => {
            #[cfg(feature = "ledger-postgres")]
            {
                match config
                    .env
                    .get("SYNAPSE_LEDGER_POSTGRES_DSN")
                    .or_else(|| config.env.get("SYNAPSE_LEDGER_DSN"))
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    Some(dsn) => super::postgres::PostgresLedger::connect(dsn)
                        .await
                        .map(|s| Arc::new(s) as Arc<dyn LedgerStore>),
                    None => Err(LedgerError::Backend(
                        "SYNAPSE_LEDGER_POSTGRES_DSN (or SYNAPSE_LEDGER_DSN) unset".into(),
                    )),
                }
            }
            #[cfg(not(feature = "ledger-postgres"))]
            {
                Err(LedgerError::Backend(
                    "ledger-postgres feature not enabled".into(),
                ))
            }
        }
        LedgerBackend::Pubsub => {
            #[cfg(feature = "ledger-pubsub")]
            {
                let project = config
                    .env
                    .get("SYNAPSE_LEDGER_PUBSUB_PROJECT")
                    .cloned()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| crate::config::vertex_project_from_env(&config.env));
                let topic = config
                    .env
                    .get("SYNAPSE_LEDGER_PUBSUB_TOPIC")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                match (project, topic) {
                    (Some(project), Some(topic)) => {
                        super::pubsub::PubsubLedger::connect(&project, topic)
                            .await
                            .map(|s| Arc::new(s) as Arc<dyn LedgerStore>)
                    }
                    _ => Err(LedgerError::Backend(
                        "SYNAPSE_LEDGER_PUBSUB_TOPIC and project env vars required for pubsub ledger"
                            .into(),
                    )),
                }
            }
            #[cfg(not(feature = "ledger-pubsub"))]
            {
                Err(LedgerError::Backend(
                    "ledger-pubsub feature not enabled".into(),
                ))
            }
        }
        LedgerBackend::Sns => {
            #[cfg(feature = "ledger-sns")]
            {
                let arn = config
                    .env
                    .get("SYNAPSE_LEDGER_SNS_TOPIC_ARN")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let region = config
                    .env
                    .get("SYNAPSE_LEDGER_SNS_REGION")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                match arn {
                    Some(arn) => super::sns::SnsLedger::connect(arn, region)
                        .await
                        .map(|s| Arc::new(s) as Arc<dyn LedgerStore>),
                    None => Err(LedgerError::Backend(
                        "SYNAPSE_LEDGER_SNS_TOPIC_ARN unset".into(),
                    )),
                }
            }
            #[cfg(not(feature = "ledger-sns"))]
            {
                Err(LedgerError::Backend(
                    "ledger-sns feature not enabled".into(),
                ))
            }
        }
    };

    match result {
        Ok(sink) => {
            tracing::info!(backend = label, "ledger sink connected");
            Some(sink)
        }
        Err(e) => {
            tracing::error!(
                backend = label,
                error = %e,
                "ledger sink connect failed; skipping"
            );
            None
        }
    }
}

/// Connect every configured ledger backend. Failures are logged per sink; the
/// gateway still starts. Returns a no-op store when nothing connects.
pub async fn build_store(config: &Config) -> Arc<dyn LedgerStore> {
    let mut sinks: Vec<(&'static str, Arc<dyn LedgerStore>)> = Vec::new();
    for &backend in &config.ledger_backends {
        if let Some(sink) = try_connect_backend(backend, config).await {
            sinks.push((backend.label(), sink));
        }
    }
    if sinks.is_empty() {
        tracing::warn!("no ledger sinks available; usage accounting disabled");
        return Arc::new(NoopLedger);
    }
    if sinks.len() == 1 {
        return sinks.into_iter().next().expect("one sink").1;
    }
    Arc::new(FanoutLedger::new(sinks))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn build_store_falls_back_to_noop_when_connect_fails() {
        let config = Config {
            addr: "0.0.0.0:8080".into(),
            metrics_addr: "0.0.0.0:9090".into(),
            routes_path: "config/routes.toml".into(),
            pricing_path: "config/pricing.toml".into(),
            guardrails_path: "config/guardrails.toml".into(),
            ledger_backends: vec![LedgerBackend::Postgres],
            default_tenant: "unattributed".into(),
            request_timeout: std::time::Duration::from_secs(120),
            stream_idle_timeout: std::time::Duration::from_secs(60),
            embed_default_input_per_mtok: 0.10,
            env: HashMap::from([(
                "SYNAPSE_LEDGER_POSTGRES_DSN".into(),
                "postgres://invalid.invalid:5432/nope".into(),
            )]),
        };
        let store = build_store(&config).await;
        // NoopLedger: record always succeeds without persisting.
        store
            .record(&crate::ledger::UsageEntry {
                ts: chrono::Utc::now(),
                tenant: "t".into(),
                workspace: None,
                user: None,
                route: "r".into(),
                provider: "p".into(),
                model: "m".into(),
                lane: "standard".into(),
                input_tokens: 1,
                output_tokens: 2,
                cost_usd: 0.0,
                request_id: "req".into(),
                status: "ok".into(),
                op: "chat".into(),
            })
            .await
            .unwrap();
    }

    #[cfg(feature = "ledger-sqlite")]
    #[tokio::test]
    async fn build_store_uses_sqlite_when_available() {
        let config = Config {
            addr: "0.0.0.0:8080".into(),
            metrics_addr: "0.0.0.0:9090".into(),
            routes_path: "config/routes.toml".into(),
            pricing_path: "config/pricing.toml".into(),
            guardrails_path: "config/guardrails.toml".into(),
            ledger_backends: vec![LedgerBackend::Sqlite],
            default_tenant: "unattributed".into(),
            request_timeout: std::time::Duration::from_secs(120),
            stream_idle_timeout: std::time::Duration::from_secs(60),
            embed_default_input_per_mtok: 0.10,
            env: HashMap::from([("SYNAPSE_LEDGER_SQLITE_DSN".into(), "sqlite::memory:".into())]),
        };
        let store = build_store(&config).await;
        let entry = crate::ledger::UsageEntry {
            ts: chrono::Utc::now(),
            tenant: "t".into(),
            workspace: None,
            user: None,
            route: "r".into(),
            provider: "p".into(),
            model: "m".into(),
            lane: "standard".into(),
            input_tokens: 3,
            output_tokens: 5,
            cost_usd: 0.001,
            request_id: "req".into(),
            status: "ok".into(),
            op: "chat".into(),
        };
        store.record(&entry).await.unwrap();
        store.record(&entry).await.unwrap();
    }
}
