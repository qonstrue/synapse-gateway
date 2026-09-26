use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::{fmt, EnvFilter};

use synapse::ai_task_type::AiTaskTypeTable;
use synapse::config::{vertex_project_from_env, Config, ProviderValidation};
use synapse::embeddings::openai::OpenAiEmbedder;
use synapse::embeddings::vertex::VertexEmbedder;
use synapse::embeddings::EmbeddingProvider;
use synapse::jev_native::JevNativeProvider;
use synapse::ledger::LedgerHandle;
use synapse::pricing::PricingTable;
use synapse::providers::vertex_auth::VertexAuth;
use synapse::providers::{unsatisfiable_providers, Catalog};
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
    // Embedding aliases live in the same file under a different top-level table.
    let embed_routes = EmbeddingRouteTable::from_toml_str(&routes_content)?;

    // Under lenient validation, drop what this process cannot serve rather than
    // refusing to start. One route table is commonly shared by a gateway and its
    // in-process consumers; a leg added for one of them should not take the
    // others down. Strict (the default) still fails fast below.
    let (routes, embed_routes) = match config.provider_validation {
        ProviderValidation::Strict => (routes, embed_routes),
        ProviderValidation::Lenient => {
            let referenced: std::collections::HashSet<String> = routes
                .referenced_providers()
                .union(&embed_routes.referenced_providers())
                .cloned()
                .collect();
            let unsatisfiable = unsatisfiable_providers(&env, &referenced);
            let drop: std::collections::HashSet<String> = unsatisfiable
                .iter()
                .map(|u| u.provider().to_string())
                .collect();
            for u in &unsatisfiable {
                tracing::warn!(provider = %u.provider(), "dropping route legs: {u}");
            }
            let pruned = routes.without_providers(&drop);
            let pruned_embed = embed_routes.without_providers(&drop);
            if !drop.is_empty() {
                tracing::warn!(
                    dropped_providers = ?{ let mut v: Vec<&str> = drop.iter().map(String::as_str).collect(); v.sort(); v },
                    aliases_before = routes.aliases().len(),
                    aliases_after = pruned.aliases().len(),
                    embedding_aliases_before = embed_routes.aliases().len(),
                    embedding_aliases_after = pruned_embed.aliases().len(),
                    "lenient provider validation pruned the route table"
                );
            }
            (pruned, pruned_embed)
        }
    };
    let pricing = PricingTable::from_toml_str(
        &std::fs::read_to_string(&config.pricing_path)
            .with_context(|| format!("reading {}", config.pricing_path))?,
    )?;

    // Optional AI task types: absent file ⇒ empty table ⇒ everything "simple".
    let ai_task_types = if std::path::Path::new(&config.ai_task_types_path).exists() {
        let content = std::fs::read_to_string(&config.ai_task_types_path)
            .with_context(|| format!("reading {}", config.ai_task_types_path))?;
        AiTaskTypeTable::from_toml_str(&content)
            .with_context(|| format!("parsing {}", config.ai_task_types_path))?
    } else {
        AiTaskTypeTable::default()
    };

    // Optional guardrails: absent file ⇒ empty engine (guardrails off).
    let guard = if std::path::Path::new(&config.guardrails_path).exists() {
        let content = std::fs::read_to_string(&config.guardrails_path)
            .with_context(|| format!("reading {}", config.guardrails_path))?;
        let cfg = synapse::guard::GuardrailsConfig::from_toml_str(&content)?;
        synapse::guard::GuardEngine::from_config(&cfg)?
    } else {
        synapse::guard::GuardEngine::empty()
    };

    // Fail-fast: build every referenced provider's client + validate creds.
    let catalog = Catalog::build(&env, &routes.referenced_providers(), config.request_timeout)?;

    // Native TypeSafe (Jev) lane is available when TYPESAFE_API_KEY is configured;
    // TYPESAFE_BASE_URL overrides the hosted endpoint (self-hosted, tests).
    let jev_native = JevNativeProvider::from_env(&env, config.request_timeout);

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

    // One embedder per referenced provider, mirroring Catalog::build creds.
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

    let store = synapse::ledger::connect::build_store(&config).await;
    let ledger = LedgerHandle::spawn(store, 10_000);

    let builder = synapse::gateway::Gateway::builder()
        .routes(routes)
        .catalog(catalog)
        .pricing(pricing)
        .ai_task_types(ai_task_types)
        .ledger(ledger)
        .vertex_native(vertex_native.map(|a| (*a).clone()))
        .jev_native(jev_native)
        .timeouts(synapse::routing::executor::StreamTimeouts {
            first_chunk: config.request_timeout,
            idle: config.stream_idle_timeout,
        })
        .default_tenant(config.default_tenant.clone())
        .guard(guard)
        .embed_routes(embed_routes)
        .embed_default_input_per_mtok(config.embed_default_input_per_mtok);
    let gateway = embedders
        .into_iter()
        .fold(builder, |b, (id, e)| b.embedder(id, e))
        .build()?;

    // A2A registry is a process-lifetime in-memory catalog. Admin + public
    // routers carry their own state; merge onto the LLM app so marketplace /
    // ploutonion hit the same stable :8080 service (not the sandbox broker).
    // Static seed is optional, like guardrails: absent file ⇒ empty registry.
    let a2a_registry = Arc::new(synapse_a2a::A2aRegistry::new());
    if std::path::Path::new(&config.a2a_path).exists() {
        synapse_a2a::seed_from_path(&a2a_registry, &config.a2a_path)
            .await
            .with_context(|| format!("seeding A2A registry from {}", config.a2a_path))?;
    } else {
        tracing::info!(
            path = %config.a2a_path,
            "a2a seed file absent; starting with empty A2A registry"
        );
    }
    let app = router(Arc::new(gateway))
        .merge(synapse_a2a::a2a_admin_router(a2a_registry.clone()))
        .merge(synapse_a2a::a2a_public_router(a2a_registry));

    tracing::info!(addr = %config.addr, "synapse-gateway listening");
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
