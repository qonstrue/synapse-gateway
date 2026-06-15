use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::{fmt, EnvFilter};

use synapse::config::{vertex_project_from_env, Config, LedgerBackend};
use synapse::embeddings::openai::OpenAiEmbedder;
use synapse::embeddings::vertex::VertexEmbedder;
use synapse::embeddings::EmbeddingProvider;
use synapse::ledger::{LedgerHandle, LedgerStore};
use synapse::pricing::PricingTable;
use synapse::providers::vertex_auth::VertexAuth;
use synapse::providers::Catalog;
use synapse::routing::embeddings::EmbeddingRouteTable;
use synapse::routing::table::RouteTable;
use synapse::server::router;
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

    let routes_content = std::fs::read_to_string(&config.routes_path)
        .with_context(|| format!("reading {}", config.routes_path))?;
    let routes = RouteTable::from_toml_str(&routes_content)?;
    let pricing = PricingTable::from_toml_str(
        &std::fs::read_to_string(&config.pricing_path)
            .with_context(|| format!("reading {}", config.pricing_path))?,
    )?;

    // Fail-fast: build every referenced provider's client + validate creds.
    let catalog = Catalog::build(&env, &routes.referenced_providers(), config.request_timeout)?;

    // Native Vertex lane is available when VERTEX_PROJECT_ID or VERTEX_PROJECT is configured.
    // Region defaults to the global endpoint; override with VERTEX_LOCATION.
    let vertex_location = env
        .get("VERTEX_LOCATION")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("global")
        .to_string();
    let vertex_native = vertex_project_from_env(&env).map(|project| {
        Arc::new(VertexNativeProvider::new(
            Arc::new(VertexAuth::from_adc()),
            project,
            vertex_location.clone(),
            config.request_timeout,
            None,
        ))
    });

    // Parse embedding aliases from the same routes file (different top-level table)
    // and build one embedder per referenced provider, mirroring Catalog::build creds.
    let embed_routes = EmbeddingRouteTable::from_toml_str(&routes_content)?;
    let mut embedders: HashMap<String, Arc<dyn EmbeddingProvider>> = HashMap::new();
    for id in embed_routes.referenced_providers() {
        let embedder: Arc<dyn EmbeddingProvider> = match id.as_str() {
            "vertex" => {
                let project = vertex_project_from_env(&env).ok_or_else(|| {
                    anyhow::anyhow!(
                        "embedding alias references provider 'vertex' but VERTEX_PROJECT_ID and VERTEX_PROJECT are unset"
                    )
                })?;
                Arc::new(VertexEmbedder::new(
                    Arc::new(VertexAuth::from_adc()),
                    project,
                    vertex_location.clone(),
                    config.request_timeout,
                ))
            }
            "openai" => {
                let api_key = env
                    .get("OPENAI_API_KEY")
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "embedding alias references provider 'openai' but OPENAI_API_KEY is unset"
                        )
                    })?;
                let base_url = env
                    .get("OPENAI_BASE_URL")
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
                Arc::new(OpenAiEmbedder::new(
                    base_url,
                    api_key,
                    config.request_timeout,
                ))
            }
            other => anyhow::bail!("embedding provider '{other}' not supported"),
        };
        embedders.insert(id, embedder);
    }

    // Build one sink per selected backend, then fan out.
    let mut sinks: Vec<(&'static str, Arc<dyn LedgerStore>)> = Vec::new();
    for backend in &config.ledger_backends {
        let sink: Arc<dyn LedgerStore> = match backend {
            LedgerBackend::Sqlite => {
                #[cfg(feature = "ledger-sqlite")]
                {
                    let dsn = config
                        .env
                        .get("SYNAPSE_LEDGER_SQLITE_DSN")
                        .or_else(|| config.env.get("SYNAPSE_LEDGER_DSN"))
                        .cloned()
                        .unwrap_or_else(|| "sqlite://synapse.db?mode=rwc".into());
                    Arc::new(synapse::ledger::sqlite::SqliteLedger::connect(&dsn).await?)
                }
                #[cfg(not(feature = "ledger-sqlite"))]
                anyhow::bail!(
                    "ledger backend 'sqlite' requested but built without the ledger-sqlite feature"
                );
            }
            LedgerBackend::Postgres => {
                #[cfg(feature = "ledger-postgres")]
                {
                    let dsn = config
                        .env
                        .get("SYNAPSE_LEDGER_POSTGRES_DSN")
                        .or_else(|| config.env.get("SYNAPSE_LEDGER_DSN"))
                        .filter(|s| !s.trim().is_empty())
                        .context("SYNAPSE_LEDGER_POSTGRES_DSN (or SYNAPSE_LEDGER_DSN) required for postgres ledger")?;
                    Arc::new(synapse::ledger::postgres::PostgresLedger::connect(dsn).await?)
                }
                #[cfg(not(feature = "ledger-postgres"))]
                anyhow::bail!("ledger backend 'postgres' requested but built without the ledger-postgres feature");
            }
            LedgerBackend::Pubsub => {
                #[cfg(feature = "ledger-pubsub")]
                {
                    let project = config
                        .env
                        .get("SYNAPSE_LEDGER_PUBSUB_PROJECT")
                        .cloned()
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| vertex_project_from_env(&config.env))
                        .context(
                            "SYNAPSE_LEDGER_PUBSUB_PROJECT, VERTEX_PROJECT_ID, or VERTEX_PROJECT required for pubsub ledger",
                        )?;
                    let topic = config
                        .env
                        .get("SYNAPSE_LEDGER_PUBSUB_TOPIC")
                        .filter(|s| !s.trim().is_empty())
                        .context("SYNAPSE_LEDGER_PUBSUB_TOPIC required for pubsub ledger")?;
                    Arc::new(synapse::ledger::pubsub::PubsubLedger::connect(project, topic).await?)
                }
                #[cfg(not(feature = "ledger-pubsub"))]
                anyhow::bail!(
                    "ledger backend 'pubsub' requested but built without the ledger-pubsub feature"
                );
            }
            LedgerBackend::Sns => {
                #[cfg(feature = "ledger-sns")]
                {
                    let arn = config
                        .env
                        .get("SYNAPSE_LEDGER_SNS_TOPIC_ARN")
                        .filter(|s| !s.trim().is_empty())
                        .context("SYNAPSE_LEDGER_SNS_TOPIC_ARN required for sns ledger")?;
                    let region = config
                        .env
                        .get("SYNAPSE_LEDGER_SNS_REGION")
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.as_str());
                    Arc::new(synapse::ledger::sns::SnsLedger::connect(arn, region).await?)
                }
                #[cfg(not(feature = "ledger-sns"))]
                anyhow::bail!(
                    "ledger backend 'sns' requested but built without the ledger-sns feature"
                );
            }
        };
        sinks.push((backend.label(), sink));
    }
    let store: Arc<dyn LedgerStore> = Arc::new(synapse::ledger::FanoutLedger::new(sinks));
    let ledger = LedgerHandle::spawn(store, 10_000);

    let builder = synapse::gateway::Gateway::builder()
        .routes(routes)
        .catalog(catalog)
        .pricing(pricing)
        .ledger(ledger)
        .vertex_native(vertex_native.map(|a| (*a).clone()))
        .timeouts(synapse::routing::executor::StreamTimeouts {
            first_chunk: config.request_timeout,
            idle: config.stream_idle_timeout,
        })
        .default_tenant(config.default_tenant.clone())
        .embed_routes(embed_routes)
        .embed_default_input_per_mtok(config.embed_default_input_per_mtok);
    let gateway = embedders
        .into_iter()
        .fold(builder, |b, (id, e)| b.embedder(id, e))
        .build()?;
    let app = router(Arc::new(gateway));
    tracing::info!(addr = %config.addr, "synapse-gateway listening");
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
