//! Starting the API: pool, migrations, listener and graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::api::{AppState, router};
use crate::config::Config;
use crate::db::postgres::{
    self, PostgresHealth, PostgresIdentity, PostgresPluginStatus, PostgresPlugins,
    PostgresStatusHistory,
};
use crate::db::repositories::Repositories;
use crate::fabric::SOURCE;
use crate::fabric::{self, Buses};
use crate::identity;
use crate::permissions;
use crate::plugins::client::H3Connector;
use crate::plugins::{self, Registry};
use doc_background_tasks::postgres::PostgresTasks;
use doc_cron_tasks::postgres::PostgresCron;

const DATABASE_WAIT: Duration = Duration::from_secs(60);

pub async fn serve(config: Config) -> Result<()> {
    let pool = postgres::pool_with_retry(&config.database, DATABASE_WAIT).await?;
    postgres::migrate(&config.database).await?;
    tracing::info!("migrations are up to date");
    let data = crate::data::postgres::PostgresData::lazy(
        config.database.plugins_url.expose(),
        config.database.max_connections,
    )?;

    let buses = match config.fabric.mode {
        crate::config::FabricMode::Memory => Buses::shared(&pool, SOURCE),
        crate::config::FabricMode::Cluster => Buses::cluster(&config).await?,
    };
    tracing::info!(mode = ?config.fabric.mode, "fabric ready");
    fabric::start(&buses).await?;

    let identities = Arc::new(PostgresIdentity::new(pool.clone()));
    let repos = Repositories {
        health: Arc::new(PostgresHealth::new(pool.clone())),
        identity: identities.clone(),
        teams: identities,
        plugins: Arc::new(PostgresPlugins::new(pool.clone())),
        plugin_status: Arc::new(PostgresPluginStatus::new(pool.clone())),
        tasks: Arc::new(PostgresTasks::new(pool.clone())),
        cron: Arc::new(PostgresCron::new(pool.clone())),
        status_history: Arc::new(PostgresStatusHistory::new(pool.clone())),
        data: Arc::new(data),
    };
    let report = identity::ensure_bootstrap(&repos, &config, &config.secrets.dir).await?;
    if report.is_empty() {
        tracing::info!("identity is already recorded");
    } else {
        tracing::info!(
            accounts = ?report.created_service_accounts,
            tokens = ?report.created_tokens,
            plugins = ?report.registered_plugins,
            "recorded the bootstrap identity"
        );
    }

    let source = Arc::new(permissions::RbacSource::new(buses.services.clone()));
    let connector = Arc::new(H3Connector::new(config.secrets.dir.clone()));
    let state = AppState::new(config.clone(), repos, buses)
        .with_permissions(source)
        .with_plugins(Registry::new(connector));
    if config.plugins.allow_rebuilds {
        tracing::warn!(
            "plugins.allow_rebuilds is on: a rebuilt plugin may register under a version it has \
             already used. This is for development only"
        );
    }
    // Before anything registers, so a plugin somebody turned off settles `cancelled`.
    plugins::switches::load(&state).await;
    plugins::switches::watch(&state);
    permissions::watch(&state);
    plugins::watch(state.clone());
    plugins::runs::start_pool(&state);
    crate::telemetry::start(&state);
    state.limits.resolve_hosts();
    plugins::service::serve(&state).await?;
    let access = doc_servicebus::Address::core("access")?;
    let handler = Arc::new(crate::api::tasks::AccessService(state.clone()));
    state.buses.services.serve(access, handler).await?;
    plugins::host::start(state.clone()).await?;

    let listener = TcpListener::bind(&config.server.http_addr)
        .await
        .with_context(|| format!("binding {}", config.server.http_addr))?
        .tap_io(|stream| {
            if let Err(err) = stream.set_nodelay(true) {
                tracing::warn!(%err, "could not set TCP_NODELAY");
            }
        });
    tracing::info!(addr = %config.server.http_addr, "backend API listening");

    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving the API")?;
    pool.close().await;
    tracing::info!("backend stopped");
    Ok(())
}

/// What a rotation did: the key everything is now under, how many secrets moved to it, and how
/// many could not be read at all and so must be set again.
pub struct Rotated {
    pub key_id: String,
    pub rotated: usize,
    pub unreadable: usize,
}

/// Makes a new settings key and re-encrypts every stored secret under it (ADR-0007). The old keys
/// stay in the volume so a value that has not moved yet can still be opened; removing them is a
/// deliberate step afterwards, once this has reported nothing left behind.
pub async fn rotate_settings_key(config: &Config, dir: &std::path::Path) -> Result<Rotated> {
    use crate::db::repositories::{PluginRepository, SettingChange};

    let before = crate::secrets::SettingsKeys::load(dir)?;
    if before.is_empty() {
        anyhow::bail!("there is no settings key to rotate; run the bootstrap first");
    }
    let id = crate::secrets::create_settings_key(dir)?;
    let keys = crate::secrets::SettingsKeys::load(dir)?;
    let current = keys.current().map(|key| key.id.clone()).unwrap_or_default();

    let pool = postgres::pool_with_retry(&config.database, DATABASE_WAIT).await?;
    let plugins = PostgresPlugins::new(pool.clone());
    let held = plugins.plugin_secrets().await.context("reading the stored secrets")?;
    let (mut rotated, mut unreadable) = (0, 0);
    for (plugin, setting) in held {
        let Some(sealed) = &setting.sealed else { continue };
        if sealed.key_id == current {
            continue;
        }
        let opened = match keys.open(&plugin, &setting.key, sealed) {
            Ok(opened) => opened,
            Err(err) => {
                tracing::warn!(plugin, key = %setting.key, %err, "this secret must be set again");
                unreadable += 1;
                continue;
            }
        };
        let resealed = keys.seal(&plugin, &setting.key, &opened)?;
        let change = SettingChange::Seal { key: setting.key.clone(), sealed: resealed };
        plugins
            .set_plugin_settings(
                &plugin,
                std::slice::from_ref(&change),
                Some("rotate-settings-key"),
            )
            .await
            .with_context(|| format!("re-encrypting {plugin}/{}", setting.key))?;
        rotated += 1;
    }
    pool.close().await;
    Ok(Rotated { key_id: id, rotated, unreadable })
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
