use std::sync::Arc;

use axum::{Router, extract::State, routing::{get, post}, Json};
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::signal;

use ub_control::control::ControlPlane;
use ub_control::member::MemberTable;
use ub_core::addr::UbAddr;
use ub_core::config::NodeConfig;
use ub_core::device::memory::MemoryDevice;
use ub_core::device::Device;
use ub_core::mr::{MrCacheTable, MrTable};
use ub_core::types::MrPerms;
use ub_dataplane::DataPlaneEngine;
use ub_fabric::udp::UdpFabric;

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
    mr_table: Arc<MrTable>,
    mr_cache: Arc<MrCacheTable>,
    dataplane: Arc<tokio::sync::Mutex<DataPlaneEngine>>,
}

#[derive(Deserialize)]
struct MrRegisterRequest {
    device_kind: String,
    len: u64,
    perms: String,
}

#[derive(Deserialize)]
struct MrDeregisterRequest {
    mr_handle: u32,
}

#[derive(Deserialize)]
struct VerbWriteRequest {
    ub_addr: String,
    data: Vec<u8>,
}

#[derive(Deserialize)]
struct VerbReadRequest {
    ub_addr: String,
    len: u32,
}

#[derive(Deserialize)]
struct VerbAtomicCasRequest {
    ub_addr: String,
    expect: u64,
    new: u64,
}

#[derive(Deserialize)]
struct VerbAtomicFaaRequest {
    ub_addr: String,
    add: u64,
}

fn parse_ub_addr(s: &str) -> Result<UbAddr, String> {
    UbAddr::from_text(s).map_err(|e| format!("invalid ub_addr: {e}"))
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

    // Create local MR table with publish event channel
    let (mr_table, mr_event_rx) = MrTable::new_with_channel(1, config.node_id);
    let mr_table = Arc::new(mr_table);

    // Start control plane
    let mut control = ControlPlane::new(Arc::clone(&config), Arc::clone(&members), Arc::clone(&mr_cache));
    control.set_mr_table(Arc::clone(&mr_table), mr_event_rx);
    control.start().await.expect("failed to start control plane");

    // Start data plane
    let data_listen: std::net::SocketAddr = config.data.listen.parse()
        .expect("invalid data listen address");
    let fabric = Arc::new(UdpFabric::bind(data_listen).await.expect("failed to bind data plane UDP socket"));
    let mut dataplane = DataPlaneEngine::new(
        config.node_id,
        Arc::clone(&mr_table),
        Arc::clone(&mr_cache),
        fabric,
    );
    dataplane.start().await.expect("failed to start data plane");

    let dataplane = Arc::new(tokio::sync::Mutex::new(dataplane));

    // Spawn a task to connect data plane to peers as they join
    let peer_change_rx = control.peer_change_rx.clone();
    let members_dp = Arc::clone(&members);
    let dataplane_peer = Arc::clone(&dataplane);
    tokio::spawn(async move {
        let mut rx = peer_change_rx;
        while rx.changed().await.is_ok() {
            let node_id = *rx.borrow();
            if node_id == 0 {
                continue;
            }
            if let Some(node) = members_dp.get(node_id) {
                if let Ok(data_addr) = node.data_addr.parse::<std::net::SocketAddr>() {
                    let dp = dataplane_peer.lock().await;
                    if let Err(e) = dp.connect_peer(node_id, data_addr).await {
                        tracing::warn!("data plane: failed to connect to peer {}: {e}", node_id);
                    }
                }
            }
        }
    });

    // Start HTTP admin API
    let state = Arc::new(AppState {
        members: Arc::clone(&members),
        config: Arc::clone(&config),
        mr_table: Arc::clone(&mr_table),
        mr_cache: Arc::clone(&mr_cache),
        dataplane,
    });

    let app = Router::new()
        .route("/admin/node/list", get(node_list))
        .route("/admin/node/info", get(node_info))
        .route("/admin/mr/list", get(mr_list))
        .route("/admin/mr/cache", get(mr_cache_list))
        .route("/admin/mr/register", post(mr_register))
        .route("/admin/mr/deregister", post(mr_deregister))
        .route("/admin/verb/write", post(verb_write))
        .route("/admin/verb/read", post(verb_read))
        .route("/admin/verb/atomic-cas", post(verb_atomic_cas))
        .route("/admin/verb/atomic-faa", post(verb_atomic_faa))
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

async fn mr_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let entries = state.mr_table.list();
    let mr_list: Vec<Value> = entries.iter().map(|e| {
        json!({
            "handle": e.handle,
            "ub_addr": format!("{}", e.base_ub_addr),
            "len": e.len,
            "perms": format!("{:?}", e.perms),
            "device_kind": format!("{:?}", e.device.kind()),
            "state": format!("{:?}", e.state()),
        })
    }).collect();
    Json(json!({ "mrs": mr_list }))
}

async fn mr_cache_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let entries = state.mr_cache.list();
    let cache_list: Vec<Value> = entries.iter().map(|e| {
        json!({
            "owner_node": e.owner_node,
            "mr_handle": e.remote_mr_handle,
            "ub_addr": format!("{}", e.base_ub_addr),
            "len": e.len,
            "perms": format!("{:?}", e.perms),
            "device_kind": format!("{:?}", e.device_kind),
        })
    }).collect();
    Json(json!({ "cache": cache_list }))
}

async fn mr_register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MrRegisterRequest>,
) -> Json<Value> {
    let perms = parse_perms(&req.perms);

    let dev: Arc<dyn Device> = match req.device_kind.as_str() {
        "npu" => {
            let size_mib = (req.len + 1024 * 1024 - 1) / (1024 * 1024); // round up to MiB
            Arc::new(ub_core::device::npu::NpuDevice::new(1, size_mib.max(1)))
        }
        _ => Arc::new(MemoryDevice::new(req.len as usize)),
    };

    match state.mr_table.register(dev, req.len, perms) {
        Ok((addr, handle)) => Json(json!({
            "mr_handle": handle.0,
            "ub_addr": format!("{}", addr),
        })),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn mr_deregister(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MrDeregisterRequest>,
) -> Json<Value> {
    match state.mr_table.deregister(ub_core::types::MrHandle(req.mr_handle)) {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_write(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbWriteRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let dp = state.dataplane.lock().await;
    match dp.ub_write_remote(addr, &req.data).await {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_read(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbReadRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let dp = state.dataplane.lock().await;
    match dp.ub_read_remote(addr, req.len).await {
        Ok(data) => Json(json!({"data": data, "len": data.len()})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_atomic_cas(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbAtomicCasRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let dp = state.dataplane.lock().await;
    match dp.ub_atomic_cas_remote(addr, req.expect, req.new).await {
        Ok(old_value) => Json(json!({"old_value": old_value})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_atomic_faa(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbAtomicFaaRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let dp = state.dataplane.lock().await;
    match dp.ub_atomic_faa_remote(addr, req.add).await {
        Ok(old_value) => Json(json!({"old_value": old_value})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

fn parse_perms(s: &str) -> MrPerms {
    let mut perms = MrPerms::empty();
    for c in s.to_lowercase().chars() {
        match c {
            'r' => perms |= MrPerms::READ,
            'w' => perms |= MrPerms::WRITE,
            'a' => perms |= MrPerms::ATOMIC,
            _ => {}
        }
    }
    perms
}
