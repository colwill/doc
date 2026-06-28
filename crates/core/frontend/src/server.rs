//! Starting the UI: bus clients, the backend client, the listener and graceful shutdown.

use anyhow::{Context, Result};
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::backend::BackendClient;
use crate::config::Config;
use crate::fabric;
use crate::web::{AppState, router};

pub async fn serve(config: Config) -> Result<()> {
    // reqwest is built without a crypto provider, so the process has to choose one.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let buses = fabric::connect(&config)?;
    tracing::info!(mode = ?config.fabric.mode, "fabric ready");
    let backend = BackendClient::new(&config.backend)?;
    let state = AppState::new(config.clone(), backend, buses);
    crate::session::watch(&state);

    let listener = TcpListener::bind(&config.server.http_addr)
        .await
        .with_context(|| format!("binding {}", config.server.http_addr))?
        .tap_io(|stream| {
            if let Err(err) = stream.set_nodelay(true) {
                tracing::warn!(%err, "could not set TCP_NODELAY");
            }
        });
    tracing::info!(
        addr = %config.server.http_addr,
        backend = %config.backend.base_url,
        assets = crate::web::assets::warm(),
        "frontend listening"
    );

    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving the UI")?;
    tracing::info!("frontend stopped");
    Ok(())
}

async fn shutdown() {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            tracing::error!(%err, "cannot listen for SIGTERM");
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
