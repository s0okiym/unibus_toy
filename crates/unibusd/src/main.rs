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
use ub_core::types::{JettyAddr, MrPerms, PeerChangeEvent};
use ub_dataplane::DataPlaneEngine;
use ub_fabric::udp::UdpFabric;
use ub_obs::metrics;
use ub_transport::manager::TransportManager;

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
    jetty_table: Arc<ub_core::jetty::JettyTable>,
    dataplane: Arc<tokio::sync::Mutex<DataPlaneEngine>>,
    prom_handle: metrics_exporter_prometheus::PrometheusHandle,
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

#[derive(Deserialize)]
struct JettyPostRecvRequest {
    jetty_handle: u32,
    len: u32,
}

#[derive(Deserialize)]
struct JettyPollCqeRequest {
    jetty_handle: u32,
}

#[derive(Deserialize)]
struct VerbSendRequest {
    dst_node_id: u16,
    dst_jetty_id: u32,
    data: Vec<u8>,
    imm: Option<u64>,
}

#[derive(Deserialize)]
struct VerbWriteImmRequest {
    ub_addr: String,
    data: Vec<u8>,
    imm: u64,
    dst_node_id: u16,
    dst_jetty_id: u32,
}

// ── KV Demo request types ─────────────────────────────────────

/// KV slot layout: key[32] + value[64] + version[8] = 104 bytes
const KV_SLOT_SIZE: usize = 104;
const KV_KEY_LEN: usize = 32;
const KV_VALUE_LEN: usize = 64;
const KV_VERSION_OFFSET: usize = 96;

#[derive(Deserialize)]
struct KvInitRequest {
    /// Number of KV slots (default 16)
    #[serde(default = "default_kv_slots")]
    slots: u32,
    /// Device kind: "memory" (default) or "npu"
    #[serde(default = "default_device_kind")]
    device_kind: String,
}

fn default_device_kind() -> String { "memory".to_string() }

fn default_kv_slots() -> u32 { 16 }

#[derive(Deserialize)]
struct KvPutRequest {
    ub_addr: String,
    slot: u32,
    key: String,
    value: String,
    /// Optional: send write_with_imm notification to replica jetty
    #[serde(default)]
    replica_node_id: Option<u16>,
    /// Optional: replica jetty ID for notification
    #[serde(default)]
    replica_jetty_id: Option<u32>,
}

#[derive(Deserialize)]
struct KvGetRequest {
    ub_addr: String,
    slot: u32,
    key: String,
}

#[derive(Deserialize)]
struct KvCasRequest {
    ub_addr: String,
    slot: u32,
    key: String,
    expect_version: u64,
    value: String,
    /// Optional: send write_with_imm notification to replica jetty on CAS success
    #[serde(default)]
    replica_node_id: Option<u16>,
    /// Optional: replica jetty ID for notification
    #[serde(default)]
    replica_jetty_id: Option<u32>,
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

    // Initialize metrics recorder
    let prom_handle = metrics::install_recorder().clone();
    metrics::describe_metrics();

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
    let jetty_table = Arc::new(ub_core::jetty::JettyTable::new(config.node_id, config.jetty.clone()));

    // Create transport manager
    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let transport = Arc::new(TransportManager::new(
        config.node_id,
        config.transport.clone(),
        config.flow.clone(),
        inbound_tx,
    ));

    let mut dataplane = DataPlaneEngine::new(
        config.node_id,
        Arc::clone(&mr_table),
        Arc::clone(&mr_cache),
        Arc::clone(&jetty_table),
        fabric,
        Arc::clone(&transport),
        inbound_rx,
    );
    dataplane.start().await.expect("failed to start data plane");

    let dataplane = Arc::new(tokio::sync::Mutex::new(dataplane));

    // Spawn a task to connect data plane to peers as they join
    let peer_change_rx = control.peer_change_rx.clone();
    let members_dp = Arc::clone(&members);
    let dataplane_peer = Arc::clone(&dataplane);
    let transport_peer = Arc::clone(&transport);
    tokio::spawn(async move {
        let mut rx = peer_change_rx;
        while rx.changed().await.is_ok() {
            let event = rx.borrow().clone();
            match event {
                PeerChangeEvent::Joined { node_id, epoch } => {
                    if node_id == 0 {
                        continue;
                    }
                    if let Some(node) = members_dp.get(node_id) {
                        if let Ok(data_addr) = node.data_addr.parse::<std::net::SocketAddr>() {
                            // Create transport session for this peer
                            let initial_credits = node.initial_credits;
                            if let Err(e) = transport_peer.create_session(node_id, epoch, initial_credits) {
                                tracing::warn!("transport: failed to create session for peer {}: {e}", node_id);
                            }
                            // Connect data plane
                            let dp = dataplane_peer.lock().await;
                            if let Err(e) = dp.connect_peer(node_id, data_addr).await {
                                tracing::warn!("data plane: failed to connect to peer {}: {e}", node_id);
                            }
                        }
                    }
                }
                PeerChangeEvent::Recovered { node_id } => {
                    tracing::info!("data plane: peer {} recovered to Active", node_id);
                    if let Some(node) = members_dp.get(node_id) {
                        if let Ok(data_addr) = node.data_addr.parse::<std::net::SocketAddr>() {
                            let dp = dataplane_peer.lock().await;
                            if let Err(e) = dp.connect_peer(node_id, data_addr).await {
                                tracing::warn!("data plane: failed to reconnect to peer {}: {e}", node_id);
                            }
                        }
                    }
                }
                PeerChangeEvent::Suspect { node_id } => {
                    tracing::warn!("data plane: peer {} is Suspect", node_id);
                }
                PeerChangeEvent::Down { node_id, epoch: _ } => {
                    tracing::warn!("data plane: peer {} is Down, disconnecting", node_id);
                    let dp = dataplane_peer.lock().await;
                    dp.disconnect_peer(node_id);
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
        jetty_table: Arc::clone(&jetty_table),
        dataplane,
        prom_handle,
    });

    let app = Router::new()
        .route("/admin/node/list", get(node_list))
        .route("/admin/node/info", get(node_info))
        .route("/admin/mr/list", get(mr_list))
        .route("/admin/mr/cache", get(mr_cache_list))
        .route("/admin/mr/register", post(mr_register))
        .route("/admin/mr/deregister", post(mr_deregister))
        .route("/admin/jetty/create", post(jetty_create))
        .route("/admin/jetty/list", get(jetty_list))
        .route("/admin/jetty/post-recv", post(jetty_post_recv))
        .route("/admin/jetty/poll-cqe", post(jetty_poll_cqe))
        .route("/admin/verb/write", post(verb_write))
        .route("/admin/verb/read", post(verb_read))
        .route("/admin/verb/atomic-cas", post(verb_atomic_cas))
        .route("/admin/verb/atomic-faa", post(verb_atomic_faa))
        .route("/admin/verb/send", post(verb_send))
        .route("/admin/verb/send-with-imm", post(verb_send_with_imm))
        .route("/admin/verb/write-imm", post(verb_write_imm))
        .route("/admin/kv/init", post(kv_init))
        .route("/admin/kv/put", post(kv_put))
        .route("/admin/kv/get", post(kv_get))
        .route("/admin/kv/cas", post(kv_cas))
        .route("/metrics", get(metrics_endpoint))
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
    let timeout_ms = state.config.mr.deregister_timeout_ms;
    match state.mr_table.deregister_async(ub_core::types::MrHandle(req.mr_handle), timeout_ms).await {
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

async fn jetty_create(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.jetty_table.create() {
        Ok(handle) => Json(json!({"jetty_handle": handle.0})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn jetty_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let infos = state.jetty_table.list();
    let list: Vec<Value> = infos.iter().map(|j| {
        json!({
            "handle": j.handle.0,
            "node_id": j.node_id,
            "jfs_depth": j.jfs_depth,
            "jfr_depth": j.jfr_depth,
            "jfc_depth": j.jfc_depth,
            "jfc_high_watermark": j.jfc_high_watermark,
            "cqe_count": j.cqe_count,
            "jfs_count": j.jfs_count,
            "jfr_count": j.jfr_count,
        })
    }).collect();
    Json(json!({ "jettys": list }))
}

async fn jetty_post_recv(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JettyPostRecvRequest>,
) -> Json<Value> {
    let jetty = match state.jetty_table.lookup(req.jetty_handle) {
        Some(j) => j,
        None => return Json(json!({"error": "jetty not found"})),
    };

    let buf = vec![0u8; req.len as usize];
    let wr_id = req.jetty_handle as u64; // simple wr_id assignment
    match jetty.post_recv(buf, wr_id) {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn jetty_poll_cqe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JettyPollCqeRequest>,
) -> Json<Value> {
    let jetty = match state.jetty_table.lookup(req.jetty_handle) {
        Some(j) => j,
        None => return Json(json!({"error": "jetty not found"})),
    };

    match jetty.poll_cqe() {
        Some(cqe) => Json(json!({
            "wr_id": cqe.wr_id,
            "status": cqe.status,
            "imm": cqe.imm,
            "byte_len": cqe.byte_len,
            "jetty_id": cqe.jetty_id,
            "verb": format!("{:?}", cqe.verb),
        })),
        None => Json(json!({"cqe": null})),
    }
}

async fn verb_send(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbSendRequest>,
) -> Json<Value> {
    let dst_jetty = JettyAddr {
        node_id: req.dst_node_id,
        jetty_id: req.dst_jetty_id,
    };

    // Use default jetty if it exists
    let src_jetty_id = state.jetty_table.default_jetty()
        .map(|h| h.0)
        .unwrap_or(0);

    let dp = state.dataplane.lock().await;
    match dp.ub_send_remote(src_jetty_id, dst_jetty, &req.data).await {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_send_with_imm(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbSendRequest>,
) -> Json<Value> {
    let dst_jetty = JettyAddr {
        node_id: req.dst_node_id,
        jetty_id: req.dst_jetty_id,
    };

    let imm = req.imm.unwrap_or(0);

    let src_jetty_id = state.jetty_table.default_jetty()
        .map(|h| h.0)
        .unwrap_or(0);

    let dp = state.dataplane.lock().await;
    match dp.ub_send_with_imm_remote(src_jetty_id, dst_jetty, &req.data, imm).await {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn verb_write_imm(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbWriteImmRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let dst_jetty = JettyAddr {
        node_id: req.dst_node_id,
        jetty_id: req.dst_jetty_id,
    };

    let src_jetty_id = state.jetty_table.default_jetty()
        .map(|h| h.0)
        .unwrap_or(0);

    let dp = state.dataplane.lock().await;
    match dp.ub_write_imm_remote(addr, &req.data, req.imm, src_jetty_id, dst_jetty).await {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

// ── KV Demo handlers ────────────────────────────────────────────

async fn kv_init(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KvInitRequest>,
) -> Json<Value> {
    let total_len = req.slots as u64 * KV_SLOT_SIZE as u64;
    let dev: Arc<dyn Device> = if req.device_kind == "npu" {
        let size_mib = (total_len + 1024 * 1024 - 1) / (1024 * 1024);
        Arc::new(ub_core::device::npu::NpuDevice::new(1, size_mib.max(1)))
    } else {
        Arc::new(MemoryDevice::new(total_len as usize))
    };
    match state.mr_table.register(dev, total_len, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC) {
        Ok((addr, handle)) => {
            // Initialize all slots: version=0, key and value zeroed
            // For direct device access, use offset 0 (each MR gets its own device)
            let entry = state.mr_table.lookup(handle.0).unwrap();
            let zero_slot = vec![0u8; KV_SLOT_SIZE];
            for i in 0..req.slots {
                let slot_base = i as u64 * KV_SLOT_SIZE as u64;
                let version_off = slot_base + KV_VERSION_OFFSET as u64;
                let _ = entry.device.write(version_off, &0u64.to_ne_bytes());
                let _ = entry.device.write(slot_base, &zero_slot);
            }
            Json(json!({
                "status": "ok",
                "mr_handle": handle.0,
                "ub_addr": format!("{}", addr),
                "slots": req.slots,
                "slot_size": KV_SLOT_SIZE,
            }))
        }
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

async fn kv_put(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KvPutRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let slot_offset = req.slot as u64 * KV_SLOT_SIZE as u64;

    // If the MR is local, use direct device access (avoids data-plane roundtrip)
    if let Some((entry, offset_in_mr)) = state.mr_table.lookup_by_addr(addr) {
        // offset_in_mr is already the correct device offset for this MR
        let version_offset = offset_in_mr + slot_offset + KV_VERSION_OFFSET as u64;
        let old_version = match entry.device.atomic_faa(version_offset, 1) {
            Ok(v) => v,
            Err(e) => return Json(json!({"error": format!("{}", e)})),
        };
        let new_version = old_version + 1;

        // Write key + value directly
        let mut kv_data = vec![0u8; KV_KEY_LEN + KV_VALUE_LEN];
        let key_bytes = req.key.as_bytes();
        let copy_len = key_bytes.len().min(KV_KEY_LEN);
        kv_data[..copy_len].copy_from_slice(&key_bytes[..copy_len]);
        let value_bytes = req.value.as_bytes();
        let copy_len = value_bytes.len().min(KV_VALUE_LEN);
        kv_data[KV_KEY_LEN..KV_KEY_LEN + copy_len].copy_from_slice(&value_bytes[..copy_len]);

        let data_offset = offset_in_mr + slot_offset;
        if let Err(e) = entry.device.write(data_offset, &kv_data) {
            return Json(json!({"error": format!("{}", e)}));
        }

        // Notify replica if configured
        if let (Some(replica_node), Some(replica_jetty)) = (req.replica_node_id, req.replica_jetty_id) {
            notify_replica(&state, replica_node, replica_jetty, req.slot).await;
        }

        Json(json!({"status": "ok", "version": new_version}))
    } else {
        // Remote MR — use data-plane verbs
        let dp = state.dataplane.lock().await;

        let version_addr = UbAddr::new(
            addr.pod_id(),
            addr.node_id(),
            addr.device_id(),
            addr.offset() + slot_offset + KV_VERSION_OFFSET as u64,
            addr.reserved(),
        );

        let old_version = match dp.ub_atomic_faa_remote(version_addr, 1).await {
            Ok(v) => v,
            Err(e) => return Json(json!({"error": format!("{}", e)})),
        };
        let new_version = old_version + 1;

        let write_addr = UbAddr::new(
            addr.pod_id(),
            addr.node_id(),
            addr.device_id(),
            addr.offset() + slot_offset,
            addr.reserved(),
        );

        let mut kv_data = vec![0u8; KV_KEY_LEN + KV_VALUE_LEN];
        let key_bytes = req.key.as_bytes();
        let copy_len = key_bytes.len().min(KV_KEY_LEN);
        kv_data[..copy_len].copy_from_slice(&key_bytes[..copy_len]);
        let value_bytes = req.value.as_bytes();
        let copy_len = value_bytes.len().min(KV_VALUE_LEN);
        kv_data[KV_KEY_LEN..KV_KEY_LEN + copy_len].copy_from_slice(&value_bytes[..copy_len]);

        match dp.ub_write_remote(write_addr, &kv_data).await {
            Ok(()) => {
                // Notify replica if configured
                if let (Some(replica_node), Some(replica_jetty)) = (req.replica_node_id, req.replica_jetty_id) {
                    drop(dp);
                    notify_replica(&state, replica_node, replica_jetty, req.slot).await;
                }
                Json(json!({"status": "ok", "version": new_version}))
            }
            Err(e) => Json(json!({"error": format!("{}", e)})),
        }
    }
}

async fn kv_get(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KvGetRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let slot_offset = req.slot as u64 * KV_SLOT_SIZE as u64;

    // Read the slot — local or remote
    let data = if let Some((entry, offset_in_mr)) = state.mr_table.lookup_by_addr(addr) {
        // Local MR — direct device read
        let mut buf = vec![0u8; KV_SLOT_SIZE];
        let offset = offset_in_mr + slot_offset;
        if let Err(e) = entry.device.read(offset, &mut buf) {
            return Json(json!({"error": format!("{}", e)}));
        }
        buf
    } else {
        // Remote MR — use data-plane read
        let read_addr = UbAddr::new(
            addr.pod_id(),
            addr.node_id(),
            addr.device_id(),
            addr.offset() + slot_offset,
            addr.reserved(),
        );
        let dp = state.dataplane.lock().await;
        match dp.ub_read_remote(read_addr, KV_SLOT_SIZE as u32).await {
            Ok(data) => data,
            Err(e) => return Json(json!({"error": format!("{}", e)})),
        }
    };

    if data.len() < KV_SLOT_SIZE {
        return Json(json!({"error": format!("read returned {} bytes, expected {}", data.len(), KV_SLOT_SIZE)}));
    }

    // Extract key, value, version
    let key_bytes = &data[..KV_KEY_LEN];
    let value_bytes = &data[KV_KEY_LEN..KV_KEY_LEN + KV_VALUE_LEN];
    let version = u64::from_ne_bytes(data[KV_VERSION_OFFSET..KV_VERSION_OFFSET + 8].try_into().unwrap_or([0; 8]));

    // Trim null bytes from key and value for display
    let key_str = String::from_utf8_lossy(key_bytes).trim_end_matches('\0').to_string();
    let value_str = String::from_utf8_lossy(value_bytes).trim_end_matches('\0').to_string();

    // Check if key matches
    let req_key_padded = pad_string(&req.key, KV_KEY_LEN);
    let key_matches = key_bytes == req_key_padded.as_slice();

    if !key_matches && version > 0 {
        return Json(json!({"error": "key mismatch", "expected_key": req.key, "found_key": key_str}));
    }

    Json(json!({
        "key": key_str,
        "value": value_str,
        "version": version,
    }))
}

async fn kv_cas(
    State(state): State<Arc<AppState>>,
    Json(req): Json<KvCasRequest>,
) -> Json<Value> {
    let addr = match parse_ub_addr(&req.ub_addr) {
        Ok(a) => a,
        Err(e) => return Json(json!({"error": e})),
    };

    let slot_offset = req.slot as u64 * KV_SLOT_SIZE as u64;
    let new_version = req.expect_version + 1;

    if let Some((entry, offset_in_mr)) = state.mr_table.lookup_by_addr(addr) {
        // Local MR — direct device access
        let version_offset = offset_in_mr + slot_offset + KV_VERSION_OFFSET as u64;
        let old_value = match entry.device.atomic_cas(version_offset, req.expect_version, new_version) {
            Ok(v) => v,
            Err(e) => return Json(json!({"error": format!("{}", e)})),
        };

        if old_value == req.expect_version {
            // CAS succeeded — write new key+value
            let mut slot_data = vec![0u8; KV_KEY_LEN + KV_VALUE_LEN];
            let key_bytes = req.key.as_bytes();
            let copy_len = key_bytes.len().min(KV_KEY_LEN);
            slot_data[..copy_len].copy_from_slice(&key_bytes[..copy_len]);
            let value_bytes = req.value.as_bytes();
            let copy_len = value_bytes.len().min(KV_VALUE_LEN);
            slot_data[KV_KEY_LEN..KV_KEY_LEN + copy_len].copy_from_slice(&value_bytes[..copy_len]);

            let data_offset = offset_in_mr + slot_offset;
            let _ = entry.device.write(data_offset, &slot_data);

            // Notify replica if configured
            if let (Some(replica_node), Some(replica_jetty)) = (req.replica_node_id, req.replica_jetty_id) {
                notify_replica(&state, replica_node, replica_jetty, req.slot).await;
            }

            Json(json!({"status": "ok", "old_version": old_value, "new_version": new_version}))
        } else {
            Json(json!({"status": "cas_failed", "current_version": old_value, "expected_version": req.expect_version}))
        }
    } else {
        // Remote MR — use data-plane verbs
        let version_addr = UbAddr::new(
            addr.pod_id(),
            addr.node_id(),
            addr.device_id(),
            addr.offset() + slot_offset + KV_VERSION_OFFSET as u64,
            addr.reserved(),
        );

        let dp = state.dataplane.lock().await;
        match dp.ub_atomic_cas_remote(version_addr, req.expect_version, new_version).await {
            Ok(old_value) => {
                if old_value == req.expect_version {
                    // CAS succeeded — write new key+value
                    let write_addr = UbAddr::new(
                        addr.pod_id(),
                        addr.node_id(),
                        addr.device_id(),
                        addr.offset() + slot_offset,
                        addr.reserved(),
                    );

                    let mut slot_data = vec![0u8; KV_KEY_LEN + KV_VALUE_LEN];
                    let key_bytes = req.key.as_bytes();
                    let copy_len = key_bytes.len().min(KV_KEY_LEN);
                    slot_data[..copy_len].copy_from_slice(&key_bytes[..copy_len]);
                    let value_bytes = req.value.as_bytes();
                    let copy_len = value_bytes.len().min(KV_VALUE_LEN);
                    slot_data[KV_KEY_LEN..KV_KEY_LEN + copy_len].copy_from_slice(&value_bytes[..copy_len]);

                    let _ = dp.ub_write_remote(write_addr, &slot_data).await;

                    // Notify replica if configured
                    if let (Some(replica_node), Some(replica_jetty)) = (req.replica_node_id, req.replica_jetty_id) {
                        drop(dp);
                        notify_replica(&state, replica_node, replica_jetty, req.slot).await;
                    }

                    Json(json!({"status": "ok", "old_version": old_value, "new_version": new_version}))
                } else {
                    Json(json!({"status": "cas_failed", "current_version": old_value, "expected_version": req.expect_version}))
                }
            }
            Err(e) => Json(json!({"error": format!("{}", e)})),
        }
    }
}

/// Notify a replica jetty about a KV slot update via write_with_imm.
async fn notify_replica(state: &Arc<AppState>, replica_node_id: u16, replica_jetty_id: u32, slot: u32) {
    let src_jetty_id = state.jetty_table.default_jetty()
        .map(|h| h.0)
        .unwrap_or(0);

    let dst_jetty = JettyAddr {
        node_id: replica_node_id,
        jetty_id: replica_jetty_id,
    };

    let dp = state.dataplane.lock().await;
    // Send an empty payload with imm = slot index as notification
    let _ = dp.ub_send_with_imm_remote(src_jetty_id, dst_jetty, &[], slot as u64).await;
}

/// Pad a string with null bytes to the specified length.
fn pad_string(s: &str, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let copy_len = s.as_bytes().len().min(len);
    buf[..copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
    buf
}

async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl axum::response::IntoResponse {
    // Update gauge metrics before rendering
    let mr_entries = state.mr_table.list();
    let mut memory_count = 0u64;
    let mut npu_count = 0u64;
    for mr in &mr_entries {
        match mr.device.kind() {
            ub_core::types::DeviceKind::Memory => memory_count += 1,
            ub_core::types::DeviceKind::Npu => npu_count += 1,
        }
    }
    ub_obs::set_gauge(ub_obs::MR_COUNT, memory_count as f64); // default (no label)
    ub_obs::set_gauge_label(ub_obs::MR_COUNT, "device", "memory", memory_count as f64);
    ub_obs::set_gauge_label(ub_obs::MR_COUNT, "device", "npu", npu_count as f64);

    let jetty_count = state.jetty_table.list().len() as f64;
    ub_obs::set_gauge(ub_obs::JETTY_COUNT, jetty_count);

    let output = state.prom_handle.render();
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
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
