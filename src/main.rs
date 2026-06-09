use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::{fmt, EnvFilter};

use synapse::config::{Config, LedgerBackend};
use synapse::ledger::{LedgerHandle, LedgerStore};
use synapse::pricing::PricingTable;
use synapse::providers::vertex_auth::VertexAuth;
use synapse::providers::Catalog;
use synapse::routing::table::RouteTable;
use synapse::server::{router, AppState};
use synapse::vertex_native::VertexNativeProvider;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install rustls CryptoProvider");
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let env: HashMap<String, String> = std::env::vars().collect();
    let config = Config::from_env_map(&env)?;

    // Install the global Prometheus recorder + pull endpoint on the metrics port.
    // Must run before any `counter!`/`histogram!` emission so metrics are recorded.
    let metrics_sockaddr: std::net::SocketAddr = config
        .metrics_addr
        .parse()
        .with_context(|| format!("parsing SYNAPSE_METRICS_ADDR '{}'", config.metrics_addr))?;
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(metrics_sockaddr)
        .install()
        .context("installing prometheus exporter")?;
    tracing::info!(addr = %config.metrics_addr, "synapse-gateway metrics listening");

    let routes = RouteTable::from_toml_str(
        &std::fs::read_to_string(&config.routes_path)
            .with_context(|| format!("reading {}", config.routes_path))?,
    )?;
    let pricing = PricingTable::from_toml_str(
        &std::fs::read_to_string(&config.pricing_path)
            .with_context(|| format!("reading {}", config.pricing_path))?,
    )?;

    // Fail-fast: build every referenced provider's client + validate creds.
    let catalog = Catalog::build(&env, &routes.referenced_providers(), config.request_timeout)?;

    // Native Vertex lane is available when VERTEX_PROJECT is configured.
    let vertex_native = env
        .get("VERTEX_PROJECT")
        .filter(|s| !s.trim().is_empty())
        .map(|project| {
            Arc::new(VertexNativeProvider::new(
                Arc::new(VertexAuth::from_adc()),
                project.clone(),
                "global".to_string(),
                config.request_timeout,
                None,
            ))
        });

    // Ledger backend selection.
    let store: Arc<dyn LedgerStore> = match config.ledger_backend {
        LedgerBackend::Sqlite => {
            #[cfg(feature = "ledger-sqlite")]
            {
                let dsn = config
                    .ledger_dsn
                    .clone()
                    .unwrap_or_else(|| "sqlite://synapse.db?mode=rwc".into());
                Arc::new(synapse::ledger::sqlite::SqliteLedger::connect(&dsn).await?)
            }
            #[cfg(not(feature = "ledger-sqlite"))]
            anyhow::bail!("ledger backend 'sqlite' requested but the binary was built without the ledger-sqlite feature");
        }
        LedgerBackend::Postgres => {
            #[cfg(feature = "ledger-postgres")]
            {
                let dsn = config
                    .ledger_dsn
                    .clone()
                    .context("SYNAPSE_LEDGER_DSN required for postgres backend")?;
                Arc::new(synapse::ledger::postgres::PostgresLedger::connect(&dsn).await?)
            }
            #[cfg(not(feature = "ledger-postgres"))]
            anyhow::bail!("ledger backend 'postgres' requested but the binary was built without the ledger-postgres feature");
        }
    };
    let ledger = LedgerHandle::spawn(store, 10_000);

    let state = AppState {
        routes: Arc::new(routes),
        catalog: Arc::new(catalog),
        pricing: Arc::new(pricing),
        ledger,
        default_tenant: config.default_tenant.clone(),
        vertex_native,
    };

    let app = router(state);
    tracing::info!(addr = %config.addr, "synapse-gateway listening");
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
