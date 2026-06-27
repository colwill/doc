//! T11 checks: values and TTLs survive the loss of a node, an expired value disappears from
//! every node, and compare-and-set rejects a stale version.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use doc_cachebus::{CacheBus, Namespace, NamespaceSpec, NetworkCacheBus};
use doc_consensus::ClusterClient;
use serde_json::json;

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Mode {
    /// Writes values that later checks read back.
    Write,
    /// Reads the values from every node, and once through the leader.
    Read,
    /// Writes a short-lived value and waits for it to expire.
    Expire,
    /// Exercises compare-and-set.
    Cas,
}

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    nodes: String,
    #[arg(long)]
    secrets: PathBuf,
    #[arg(long)]
    mode: Mode,
    #[arg(long, default_value = "core.drive")]
    namespace: String,
    #[arg(long, default_value_t = 20)]
    count: u64,
}

fn client(nodes: &str, secrets: &Path) -> Result<NetworkCacheBus> {
    let token =
        std::fs::read_to_string(secrets.join("tokens/buses/cachebus.token"))?.trim().to_string();
    let nodes: Vec<String> = nodes.split(',').map(str::to_string).collect();
    Ok(NetworkCacheBus::new(ClusterClient::new("cachebus", nodes, token, secrets.to_path_buf())?))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let bus = client(&cli.nodes, &cli.secrets)?;
    let namespace = Namespace::new(cli.namespace.clone())?;

    match cli.mode {
        Mode::Write => {
            bus.register_namespace(NamespaceSpec::new(
                namespace.clone(),
                Some(Duration::from_secs(600)),
            ))
            .await?;
            for index in 0..cli.count {
                bus.set(&namespace, &format!("key-{index}"), json!({"index": index}), None).await?;
            }
            println!("{}", json!({"written": cli.count}));
        }
        Mode::Read => {
            let mut per_node = serde_json::Map::new();
            for node in cli.nodes.split(',') {
                let single = client(node, &cli.secrets)?;
                let mut found = 0;
                let mut with_ttl = 0;
                for index in 0..cli.count {
                    if let Some(entry) = single.get(&namespace, &format!("key-{index}")).await? {
                        found += 1;
                        if entry.expires_at.is_some() {
                            with_ttl += 1;
                        }
                    }
                }
                per_node.insert(node.to_string(), json!({"found": found, "with_ttl": with_ttl}));
            }
            let leader = bus.get_consistent(&namespace, "key-0").await?;
            println!(
                "{}",
                json!({
                    "expected": cli.count,
                    "per_node": per_node,
                    "consistent_read": leader.map(|entry| entry.value),
                })
            );
        }
        Mode::Expire => {
            bus.register_namespace(NamespaceSpec::new(namespace.clone(), None)).await?;
            bus.set(&namespace, "short", json!("gone soon"), Some(Duration::from_secs(2))).await?;
            let before = bus.get(&namespace, "short").await?.is_some();
            tokio::time::sleep(Duration::from_secs(4)).await;
            let mut still_there = Vec::new();
            for node in cli.nodes.split(',') {
                let single = client(node, &cli.secrets)?;
                if single.get(&namespace, "short").await?.is_some() {
                    still_there.push(node.to_string());
                }
            }
            println!(
                "{}",
                json!({
                    "present_before_expiry": before,
                    "nodes_still_holding_it": still_there,
                })
            );
        }
        Mode::Cas => {
            bus.register_namespace(NamespaceSpec::new(namespace.clone(), None)).await?;
            bus.delete(&namespace, "counter").await?;
            let created = bus.compare_and_set(&namespace, "counter", None, json!(1), None).await?;
            let stale = bus.compare_and_set(&namespace, "counter", None, json!(2), None).await?;
            let updated = bus
                .compare_and_set(
                    &namespace,
                    "counter",
                    created.as_ref().map(|entry| entry.version),
                    json!(3),
                    None,
                )
                .await?;
            let value = bus.get_consistent(&namespace, "counter").await?;
            println!(
                "{}",
                json!({
                    "created_version": created.map(|entry| entry.version),
                    "rejected_when_stale": stale.is_none(),
                    "updated_version": updated.map(|entry| entry.version),
                    "value": value.map(|entry| entry.value),
                })
            );
        }
    }
    Ok(())
}
