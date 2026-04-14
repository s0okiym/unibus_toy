use std::sync::Arc;

use axum::{Router, extract::State, routing::get, Json};
use clap::Parser;
use serde_json::{json, Value};
use tokio::signal;

use ub_control::control::ControlPlane;
use ub_control::member::MemberTable;
use ub_core::config::NodeConfig;
use ub_core::mr::MrCacheTable;

#[derive(Parser)]
#[command(name = "unibusd", about = "UniBus daemon")]
struct Cli {
    /// Path to YAML configuration file
    #[arg(short, long)]
    config: String,
}

/// Shared application state.
struct AppState {
    members: Arc<MemberTable>,
    config: Arc<NodeConfig>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load config
    let config_str = std::fs::read_to_string(&cli.config)
        .expect("failed to read config file");
    let config: NodeConfig = serde_yaml::from_str(&config_str)
        .expect("failed to parse config");
    let config = Arc::new(config);

    tracing::info!("starting unibusd node_id={}", config.node_id);

    // Initialize subsystems
    let members = Arc::new(MemberTable::new(config.node_id));
    let mr_cache = Arc::new(MrCacheTable::new());

    // Start control plane
    let mut control = ControlPlane::new(Arc::clone(&config), Arc::clone(&members), Arc::clone(&mr_cache));
    control.start().await.expect("failed to start control plane");

    // Start HTTP admin API
    let state = Arc::new(AppState {
        members: Arc::clone(&members),
        config: Arc::clone(&config),
    });

    let app = Router::new()
        .route("/admin/node/list", get(node_list))
        .route("/admin/node/info", get(node_info))
        .with_state(state);

    let metrics_addr: std::net::SocketAddr = config.obs.metrics_listen.parse()
        .expect("invalid metrics_listen address");
    tracing::info!("admin API listening on {}", metrics_addr);

    let listener = tokio::net::TcpListener::bind(metrics_addr).await.unwrap();
    let admin_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Wait for shutdown signal
    tracing::info!("unibusd is running. Press Ctrl+C to stop.");
    signal::ctrl_c().await.expect("failed to listen for ctrl+c");
    tracing::info!("shutting down...");

    control.shutdown().await;
    admin_handle.abort();

    tracing::info!("unibusd stopped.");
}

async fn node_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let nodes = state.members.list();
    let node_list: Vec<Value> = nodes.iter().map(|n| {
        json!({
            "node_id": n.node_id,
            "state": n.state.to_string(),
            "data_addr": n.data_addr,
            "epoch": n.epoch,
            "initial_credits": n.initial_credits,
        })
    }).collect();
    Json(json!({ "nodes": node_list }))
}

async fn node_info(State(state): State<Arc<AppState>>) -> Json<Value> {
    // For now, return info about the local node
    let local_id = state.members.local_node_id();
    match state.members.get(local_id) {
        Some(n) => Json(json!({
            "node_id": n.node_id,
            "state": n.state.to_string(),
            "control_addr": n.control_addr,
            "data_addr": n.data_addr,
            "epoch": n.epoch,
            "initial_credits": n.initial_credits,
        })),
        None => Json(json!({"error": "node not found"})),
    }
}
