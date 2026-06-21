//! T08 check: a three-node cluster with a counter state machine that keeps accepting writes
//! while any one node is stopped. `node` runs a member; `drive` writes and reports.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use doc_consensus::node::{BusService, DEFAULT_DEADLINE, node_error};
use doc_consensus::types::NodeId;
use doc_consensus::{BusStateMachine, ClusterClient, Node, NodeConfig, Tuning};
use doc_transport::{H3ServerStream, read_body, respond_json};
use http::{Request, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Command {
    Increment(i64),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Response {
    value: i64,
}

#[derive(Default)]
struct Counter {
    value: i64,
}

impl BusStateMachine for Counter {
    type Command = Command;
    type Response = Response;
    type Snapshot = i64;

    fn apply(&mut self, _log_index: u64, command: Command) -> Response {
        match command {
            Command::Increment(by) => self.value += by,
        }
        Response { value: self.value }
    }

    fn snapshot(&self) -> i64 {
        self.value
    }

    fn restore(&mut self, snapshot: i64) {
        self.value = snapshot;
    }
}

struct CounterRoutes {
    node: Arc<Node<Counter>>,
}

impl BusService for CounterRoutes {
    async fn handle(&self, request: Request<()>, mut stream: H3ServerStream) -> Result<()> {
        let path = request.uri().path().to_string();
        let body = read_body(&mut stream).await?;
        let (status, value) = match path.as_str() {
            "/counter/v1/increment" => {
                let by: i64 = serde_json::from_slice(&body).unwrap_or(1);
                match self.node.write(Command::Increment(by)).await {
                    Ok(applied) => (
                        StatusCode::OK,
                        json!({"value": applied.response.value, "index": applied.index}),
                    ),
                    Err(err) => node_error(err),
                }
            }
            "/counter/v1/value" => {
                let value = self.node.state().read(|counter| counter.value);
                (StatusCode::OK, json!({"value": value, "stale": true}))
            }
            "/counter/v1/value-linearizable" => match self.node.ensure_linearizable().await {
                Ok(()) => {
                    let value = self.node.state().read(|counter| counter.value);
                    (StatusCode::OK, json!({"value": value, "stale": false}))
                }
                Err(err) => node_error(err),
            },
            _ => (StatusCode::NOT_FOUND, json!({"error": "unknown route"})),
        };
        respond_json(&mut stream, status, &value).await
    }
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Mode,
}

#[derive(clap::Args)]
struct NodeArgs {
    #[arg(long)]
    id: NodeId,
    #[arg(long)]
    bind: SocketAddr,
    #[arg(long)]
    advertise: String,
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    secrets: PathBuf,
    #[arg(long)]
    certificate: String,
    #[arg(long, env = "BUS_TOKEN")]
    token: String,
    /// `id=host:port`, repeated; the cluster is created on first start.
    #[arg(long = "peer")]
    peers: Vec<String>,
}

#[derive(Subcommand)]
enum Mode {
    Node(NodeArgs),
    Drive {
        #[arg(long)]
        nodes: String,
        #[arg(long)]
        secrets: PathBuf,
        #[arg(long, env = "BUS_TOKEN")]
        token: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long, default_value_t = 500)]
        window_ms: u64,
    },
}

async fn run_node(args: NodeArgs) -> Result<()> {
    let peers = args
        .peers
        .iter()
        .filter_map(|peer| peer.split_once('='))
        .map(|(id, addr)| Ok::<_, anyhow::Error>((id.parse::<NodeId>()?, addr.to_string())))
        .collect::<Result<Vec<_>>>()?;
    let config = NodeConfig {
        id: args.id,
        bus: "counter".into(),
        bind: args.bind,
        advertise: args.advertise,
        data_dir: args.data,
        secrets: args.secrets,
        certificate_name: args.certificate,
        token: args.token.into(),
        peers,
        tuning: Tuning::default(),
    };
    let node = Node::start(config, Counter::default()).await?;
    let routes = Arc::new(CounterRoutes { node: node.clone() });
    let serving = tokio::spawn(node.clone().serve(routes));
    tokio::time::sleep(Duration::from_millis(500)).await;
    node.ensure_cluster().await?;
    serving.await?;
    Ok(())
}

async fn drive(
    nodes: String,
    secrets: PathBuf,
    token: String,
    seconds: u64,
    window_ms: u64,
) -> Result<()> {
    let addresses: Vec<String> = nodes.split(',').map(str::to_string).collect();
    let client = ClusterClient::new("counter", addresses, token, secrets)?;
    let started = Instant::now();
    let stop = started + Duration::from_secs(seconds);
    let (mut acked, mut failed, mut longest_gap) = (0u64, 0u64, Duration::ZERO);
    let (mut window, mut window_start, mut last_ok) = ((0u64, 0u64), 0u64, started);
    while Instant::now() < stop {
        let at = started.elapsed().as_millis() as u64 / window_ms * window_ms;
        if at != window_start {
            println!("{}", json!({"window_ms": window_start, "ok": window.0, "failed": window.1}));
            window = (0, 0);
            window_start = at;
        }
        match client.call::<_, Value>("/counter/v1/increment", &1, DEFAULT_DEADLINE).await {
            Ok(_) => {
                acked += 1;
                window.0 += 1;
                let now = Instant::now();
                longest_gap = longest_gap.max(now.duration_since(last_ok));
                last_ok = now;
            }
            Err(_) => {
                failed += 1;
                window.1 += 1;
            }
        }
    }
    let value: Value = client
        .call("/counter/v1/value-linearizable", &json!({}), DEFAULT_DEADLINE)
        .await
        .unwrap_or(json!({"value": null}));
    println!(
        "{}",
        json!({
            "summary": true,
            "acked": acked,
            "failed": failed,
            "linearizable_value": value["value"],
            "longest_gap_ms": longest_gap.as_millis() as u64,
        })
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,openraft=warn,quiche=warn,tokio_quiche=warn".into()),
        )
        .with_target(false)
        .init();
    match Cli::parse().command {
        Mode::Node(args) => run_node(args).await,
        Mode::Drive { nodes, secrets, token, seconds, window_ms } => {
            drive(nodes, secrets, token, seconds, window_ms).await
        }
    }
}
