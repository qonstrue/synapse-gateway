use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use synapse_proxy::admin::admin_router;
use synapse_proxy::build_router;
use synapse_proxy::config::Config;
use synapse_proxy::http_client;
use synapse_proxy::metrics::{metrics_router, Metrics};
use synapse_proxy::proxy::AppState;
use synapse_proxy::ProxyBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install rustls CryptoProvider");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load()?;
    let built = ProxyBuilder::from_config(config).build()?;
    let (metrics, registry) = Metrics::new()?;
    let http_client = http_client::build_http_client()?;

    let shutting_down = Arc::new(AtomicBool::new(false));
    let state = AppState {
        routes: Arc::new(built.routes),
        context: built.context.clone(),
        client: http_client,
        shutting_down: shutting_down.clone(),
        metrics: Arc::new(metrics),
    };

    let data = build_router(state);
    let admin = admin_router(built.context);
    let metrics_app = metrics_router(registry);

    let data_l = tokio::net::TcpListener::bind(&built.addr).await?;
    let admin_l = tokio::net::TcpListener::bind(&built.admin_addr).await?;
    let metrics_l = tokio::net::TcpListener::bind(&built.metrics_addr).await?;
    tracing::info!(data = %built.addr, admin = %built.admin_addr, metrics = %built.metrics_addr, "synapse-proxy listening");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Signal task: flip readiness flag, drain briefly, then broadcast shutdown to all servers.
    let sd = shutting_down.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("synapse-proxy draining");
        sd.store(true, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _ = shutdown_tx.send(true);
    });

    let data_srv =
        axum::serve(data_l, data).with_graceful_shutdown(shutdown_wait(shutdown_rx.clone()));
    let admin_srv =
        axum::serve(admin_l, admin).with_graceful_shutdown(shutdown_wait(shutdown_rx.clone()));
    let metrics_srv = axum::serve(metrics_l, metrics_app)
        .with_graceful_shutdown(shutdown_wait(shutdown_rx.clone()));

    tokio::try_join!(
        async { data_srv.await.map_err(anyhow::Error::from) },
        async { admin_srv.await.map_err(anyhow::Error::from) },
        async { metrics_srv.await.map_err(anyhow::Error::from) },
    )?;
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl-c");
    };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

async fn shutdown_wait(mut rx: tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}
