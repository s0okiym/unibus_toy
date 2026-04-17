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
use ub_core::types::{AccessPattern, AllocHints, CapacityClass, DeviceProfile, JettyAddr, LatencyClass, MrPerms, PeerChangeEvent, RegionId, RegionInfo, RegionState, UbVa};
use ub_dataplane::DataPlaneEngine;
use ub_fabric::udp::UdpFabric;
use ub_managed::{CachePool, CoherenceManager, DeviceRegistry, FetchAgent, Placer, RegionTable, SubAllocator};
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
    device_registry: Arc<DeviceRegistry>,
    placer: Arc<Placer>,
    region_table: Arc<RegionTable>,
    fetch_agent: Arc<FetchAgent>,
    coherence: Arc<CoherenceManager>,
    /// MR handle for the local memory pool MR (used for managed layer regions).
    pool_mr_handle: Option<u32>,
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

// ── Managed Layer request types ─────────────────────────────

#[derive(Deserialize)]
struct RegionAllocRequest {
    /// Size in bytes to allocate
    size: u64,
    /// Latency class: "critical", "normal", "bulk" (default: "normal")
    #[serde(default = "default_latency_class")]
    latency_class: String,
    /// Capacity class: "small", "large", "huge" (default: "small")
    #[serde(default = "default_capacity_class")]
    capacity_class: String,
    /// Pin to device kind: "memory", "npu", or empty (default: none)
    #[serde(default)]
    pin: Option<String>,
}

fn default_latency_class() -> String { "normal".to_string() }
fn default_capacity_class() -> String { "small".to_string() }

#[derive(Deserialize)]
struct RegionFreeRequest {
    /// Region ID to free
    region_id: u64,
}

#[derive(Deserialize)]
struct VerbReadVaRequest {
    /// Virtual address (hex u128)
    va: String,
    /// Offset within the region
    #[serde(default)]
    offset: u64,
    /// Number of bytes to read
    len: u32,
}

#[derive(Deserialize)]
struct VerbWriteVaRequest {
    /// Virtual address (hex u128)
    va: String,
    /// Offset within the region
    #[serde(default)]
    offset: u64,
    /// Data to write
    data: Vec<u8>,
}

#[derive(Deserialize)]
struct AcquireWriterRequest {
    /// Virtual address (hex u128)
    va: String,
}

#[derive(Deserialize)]
struct ReleaseWriterRequest {
    /// Virtual address (hex u128)
    va: String,
}

#[derive(Deserialize)]
struct RegionInvalidateRequest {
    /// Region ID to invalidate
    region_id: u64,
    /// New epoch from the writer
    new_epoch: u64,
}

#[derive(Deserialize)]
struct RegionRegisterRemoteRequest {
    /// Region ID
    region_id: u64,
    /// Home node ID
    home_node_id: u16,
    /// Device ID on the home node
    device_id: u16,
    /// MR handle on the home node
    mr_handle: u32,
    /// Base offset within the home MR
    base_offset: u64,
    /// Region length in bytes
    len: u64,
    /// Current epoch
    #[serde(default)]
    epoch: u64,
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

    // Initialize device registry (Managed layer) and register local device profiles
    let device_registry = Arc::new(DeviceRegistry::new(config.managed.clone()));
    let mut next_device_id: u16 = 0;
    {
        // Register local devices from MR table
        let mr_entries = mr_table.list();
        for mr in &mr_entries {
            let profile = DeviceProfile {
                device_key: (config.node_id, next_device_id),
                kind: mr.device.kind(),
                tier: mr.device.tier(),
                capacity_bytes: mr.len,
                peak_read_bw_mbps: mr.device.peak_read_bw_mbps(),
                peak_write_bw_mbps: mr.device.peak_write_bw_mbps(),
                read_latency_ns_p50: mr.device.read_latency_ns_p50(),
                write_latency_ns_p50: mr.device.write_latency_ns_p50(),
                used_bytes: 0,
                recent_rps: 0,
            };
            device_registry.register(profile);
            next_device_id += 1;
        }
    }

    // Initialize Placer and RegionTable (Managed layer)
    let placer = Arc::new(Placer::new(Arc::clone(&device_registry), config.managed.clone()));
    let region_table = Arc::new(RegionTable::new());

    // If managed is enabled, register pool MRs and set up sub-allocators
    let mut pool_mr_handle: Option<u32> = None;
    if config.managed.enabled {
        let pool_config = &config.managed.pool;

        // Register a memory pool MR
        let memory_pool_size = 1024 * 1024 * 256; // 256 MiB default pool
        let memory_dev: Arc<dyn Device> = Arc::new(MemoryDevice::new(memory_pool_size as usize));
        let memory_pool_reserve = (memory_pool_size as f64 * pool_config.memory_reserve_ratio) as u64;
        let memory_pool_usable = memory_pool_size - memory_pool_reserve;

        if let Ok((_addr, handle)) = mr_table.register(memory_dev, memory_pool_size, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC) {
            pool_mr_handle = Some(handle.0);
            let pool_device_id = next_device_id;
            #[allow(unused_assignments)]
            { next_device_id += 1; }

            let sub_alloc = SubAllocator::new(0, memory_pool_usable);
            placer.register_sub_allocator(config.node_id, pool_device_id, sub_alloc);

            // Register a device profile for the pool device
            let pool_profile = DeviceProfile {
                device_key: (config.node_id, pool_device_id),
                kind: ub_core::types::DeviceKind::Memory,
                tier: ub_core::types::StorageTier::Warm,
                capacity_bytes: memory_pool_usable,
                peak_read_bw_mbps: 10_000,
                peak_write_bw_mbps: 10_000,
                read_latency_ns_p50: 200,
                write_latency_ns_p50: 200,
                used_bytes: 0,
                recent_rps: 0,
            };
            device_registry.register(pool_profile);
        }

        tracing::info!("managed layer enabled: pool MRs registered");
    }

    // Initialize FetchAgent with cache pool (with eviction notification to RegionTable)
    let cache_pool = Arc::new(CachePool::with_region_table(
        SubAllocator::new(0, config.managed.cache.max_bytes),
        config.managed.cache.max_bytes,
        Arc::clone(&region_table),
    ));
    let fetch_agent = Arc::new(FetchAgent::new(Arc::clone(&region_table), Arc::clone(&cache_pool)));

    // Initialize CoherenceManager (SWMR protocol)
    let coherence = Arc::new(CoherenceManager::new(config.managed.writer_lease_ms));

    // Spawn writer lease expiry checker
    let coherence_timer = Arc::clone(&coherence);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            interval.tick().await;
            let expired = coherence_timer.check_lease_expiry();
            for (region_id, writer_node_id) in expired {
                tracing::info!(
                    "writer lease expired: region_id={}, writer_node_id={}",
                    region_id.0, writer_node_id
                );
            }
        }
    });

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
        dataplane: Arc::clone(&dataplane),
        device_registry: Arc::clone(&device_registry),
        placer: Arc::clone(&placer),
        region_table: Arc::clone(&region_table),
        fetch_agent: Arc::clone(&fetch_agent),
        coherence: Arc::clone(&coherence),
        pool_mr_handle,
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
        .route("/admin/device/list", get(device_list))
        .route("/admin/region/alloc", post(region_alloc))
        .route("/admin/region/free", post(region_free))
        .route("/admin/region/list", get(region_list))
        .route("/admin/region/register-remote", post(region_register_remote))
        .route("/admin/region/invalidate", post(region_invalidate))
        .route("/admin/verb/read-va", post(verb_read_va))
        .route("/admin/verb/write-va", post(verb_write_va))
        .route("/admin/verb/acquire-writer", post(acquire_writer))
        .route("/admin/verb/release-writer", post(release_writer))
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

async fn device_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let profiles = state.device_registry.list();
    let list: Vec<Value> = profiles.iter().map(|p| {
        json!({
            "node_id": p.device_key.0,
            "device_id": p.device_key.1,
            "kind": format!("{:?}", p.kind),
            "tier": format!("{:?}", p.tier),
            "capacity_bytes": p.capacity_bytes,
            "used_bytes": p.used_bytes,
            "free_bytes": p.free_bytes(),
            "free_ratio": format!("{:.2}", p.free_ratio()),
            "peak_read_bw_mbps": p.peak_read_bw_mbps,
            "peak_write_bw_mbps": p.peak_write_bw_mbps,
            "read_latency_ns_p50": p.read_latency_ns_p50,
            "write_latency_ns_p50": p.write_latency_ns_p50,
            "recent_rps": p.recent_rps,
        })
    }).collect();
    Json(json!({ "devices": list }))
}

// ── Managed Layer handlers ─────────────────────────────────────

async fn region_alloc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegionAllocRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let latency_class = match req.latency_class.as_str() {
        "critical" => LatencyClass::Critical,
        "bulk" => LatencyClass::Bulk,
        _ => LatencyClass::Normal,
    };
    let capacity_class = match req.capacity_class.as_str() {
        "large" => CapacityClass::Large,
        "huge" => CapacityClass::Huge,
        _ => CapacityClass::Small,
    };
    let pin = match req.pin.as_deref() {
        Some("memory") => Some(ub_core::types::DeviceKind::Memory),
        Some("npu") => Some(ub_core::types::DeviceKind::Npu),
        _ => None,
    };

    let hints = AllocHints {
        size: req.size,
        access: AccessPattern::Mixed,
        latency_class,
        capacity_class,
        pin,
        expected_readers: 0,
    };

    match state.placer.place(&hints, state.config.node_id) {
        Ok((va, placement)) => {
            // Determine the MR handle: use the pool MR handle if this is a local home region
            let mr_handle = if placement.home_node_id == state.config.node_id {
                state.pool_mr_handle.unwrap_or(placement.mr_handle)
            } else {
                placement.mr_handle
            };
            // Update the placement's mr_handle
            state.placer.set_mr_handle(va.region_id(), mr_handle);

            // Record in the region table as Home
            let region_info = RegionInfo {
                region_id: va.region_id(),
                home_node_id: placement.home_node_id,
                device_id: placement.device_id,
                mr_handle,
                base_offset: placement.base_offset,
                len: placement.len,
                epoch: 0,
                state: RegionState::Home,
                local_mr_handle: None,
            };
            state.region_table.insert(region_info);

            // Register with CoherenceManager for SWMR tracking
            state.coherence.register_region(
                va.region_id(),
                placement.mr_handle,
                placement.base_offset,
                placement.len,
            );

            ub_obs::incr(ub_obs::PLACEMENT_DECISION);

            // Record tier label for placement decision
            ub_obs::incr_label(ub_obs::PLACEMENT_DECISION, "tier", "warm");

            Json(json!({
                "status": "ok",
                "va": format!("{:032x}", va.0),
                "region_id": va.region_id().0,
                "home_node_id": placement.home_node_id,
                "device_id": placement.device_id,
                "mr_handle": placement.mr_handle,
                "base_offset": placement.base_offset,
                "len": placement.len,
            }))
        }
        Err(e) => Json(json!({"error": e})),
    }
}

async fn region_free(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegionFreeRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let region_id = RegionId(req.region_id);
    match state.placer.free(region_id) {
        Some(_placement) => {
            state.region_table.remove(region_id);
            state.coherence.unregister_region(region_id);
            Json(json!({"status": "ok"}))
        }
        None => Json(json!({"error": "region not found"})),
    }
}

/// Register a remote region in the local region table.
/// This simulates what the control plane would do in a real distributed system:
/// when a region is created, the placer notifies other nodes so they can
/// discover the region for future reads.
async fn region_register_remote(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegionRegisterRemoteRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let region_info = RegionInfo {
        region_id: RegionId(req.region_id),
        home_node_id: req.home_node_id,
        device_id: req.device_id,
        mr_handle: req.mr_handle,
        base_offset: req.base_offset,
        len: req.len,
        epoch: req.epoch,
        state: RegionState::Invalid, // Not yet cached — will become Shared on first FETCH
        local_mr_handle: None,
    };
    state.region_table.insert(region_info);

    Json(json!({
        "status": "ok",
        "region_id": req.region_id,
        "state": "Invalid",
    }))
}

/// Handle an INVALIDATE notification from the home node.
/// Invalidates the local cache entry for the given region.
async fn region_invalidate(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegionInvalidateRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let region_id = RegionId(req.region_id);
    let invalidated = state.fetch_agent.receive_invalidate(region_id, req.new_epoch);
    if invalidated {
        Json(json!({"status": "ok", "region_id": req.region_id, "new_epoch": req.new_epoch, "invalidated": true}))
    } else {
        Json(json!({"status": "ok", "region_id": req.region_id, "new_epoch": req.new_epoch, "invalidated": false, "reason": "not cached or stale"}))
    }
}

async fn region_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let regions = state.region_table.list();
    let list: Vec<Value> = regions.iter().map(|r| {
        // Add coherence info (writer, readers) if this is a home region
        let (writer, readers) = if r.state == RegionState::Home && state.coherence.contains_region(r.region_id) {
            let w = state.coherence.get_writer(r.region_id).map(|info| info.writer_node_id);
            let rs: Vec<u16> = state.coherence.get_readers(r.region_id).into_iter().collect();
            (w, rs)
        } else {
            (None, Vec::new())
        };
        json!({
            "region_id": r.region_id.0,
            "home_node_id": r.home_node_id,
            "device_id": r.device_id,
            "mr_handle": r.mr_handle,
            "base_offset": r.base_offset,
            "len": r.len,
            "epoch": r.epoch,
            "state": format!("{:?}", r.state),
            "local_mr_handle": r.local_mr_handle,
            "writer_node_id": writer,
            "readers": readers,
        })
    }).collect();
    Json(json!({ "regions": list }))
}

async fn verb_read_va(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbReadVaRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let va = match parse_va(&req.va) {
        Ok(v) => v,
        Err(e) => return Json(json!({"error": e})),
    };

    let region_id = va.region_id();
    let offset_in_region = va.offset() + req.offset;

    // Use FetchAgent for read-through cache logic
    match state.fetch_agent.read_va(region_id) {
        ub_managed::fetch_agent::ReadVaResult::Home(region) => {
            // Read directly from local MR
            let offset = region.base_offset + offset_in_region;
            if let Some(entry) = state.mr_table.lookup(region.mr_handle) {
                let mut buf = vec![0u8; req.len as usize];
                match entry.device.read(offset, &mut buf) {
                    Ok(()) => Json(json!({"data": buf, "len": buf.len(), "source": "home"})),
                    Err(e) => Json(json!({"error": format!("{}", e)})),
                }
            } else {
                Json(json!({"error": "home MR not found"}))
            }
        }
        ub_managed::fetch_agent::ReadVaResult::CacheHit(_pool_offset, _region) => {
            // Read from cache pool MR — use the pool MR handle
            let cache_entry = state.fetch_agent.cache_pool().lookup(region_id).unwrap();
            let read_offset = cache_entry.pool_offset + offset_in_region;
            if read_offset + req.len as u64 > cache_entry.pool_offset + cache_entry.len {
                return Json(json!({"error": "read beyond cached region bounds"}));
            }
            // Use the pool MR handle to find the right MR
            let pool_handle = state.pool_mr_handle.unwrap_or(0);
            if let Some(entry) = state.mr_table.lookup(pool_handle) {
                let mut buf = vec![0u8; req.len as usize];
                match entry.device.read(read_offset, &mut buf) {
                    Ok(()) => Json(json!({"data": buf, "len": buf.len(), "source": "cache"})),
                    Err(e) => Json(json!({"error": format!("{}", e)})),
                }
            } else {
                Json(json!({"error": "pool MR not found"}))
            }
        }
        ub_managed::fetch_agent::ReadVaResult::CacheMiss(region) => {
            // FETCH the full region from home node via data plane, then cache and read
            let ub_addr = UbAddr::new(
                state.config.pod_id,
                region.home_node_id,
                region.device_id,
                region.base_offset,
                0,
            );
            let dp = state.dataplane.lock().await;
            match dp.ub_read_remote(ub_addr, region.len as u32).await {
                Ok(full_data) => {
                    drop(dp);
                    // Cache the full region data.
                    // Use the region table's current epoch (not +1) — the epoch is
                    // authoritative and the home node's acquire_writer/release_writer
                    // cycle handles epoch bumping.
                    if let Ok(pool_offset) = state.fetch_agent.complete_fetch(
                        region_id,
                        region.len,
                        region.epoch,
                    ) {
                        // Write the fetched data into the pool MR at the allocated offset
                        if let Some(pool_handle) = state.pool_mr_handle {
                            if let Some(entry) = state.mr_table.lookup(pool_handle) {
                                let _ = entry.device.write(pool_offset, &full_data);
                            }
                        }
                    }
                    // Extract the requested portion from the full data
                    let start = offset_in_region as usize;
                    let end = (start + req.len as usize).min(full_data.len());
                    let data = if start < full_data.len() {
                        full_data[start..end].to_vec()
                    } else {
                        vec![0u8; req.len as usize]
                    };
                    Json(json!({"data": data, "len": data.len(), "source": "fetch"}))
                }
                Err(e) => Json(json!({"error": format!("{}", e)})),
            }
        }
        ub_managed::fetch_agent::ReadVaResult::UnknownRegion => {
            Json(json!({"error": "region not found"}))
        }
    }
}

async fn verb_write_va(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerbWriteVaRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let va = match parse_va(&req.va) {
        Ok(v) => v,
        Err(e) => return Json(json!({"error": e})),
    };

    let region_id = va.region_id();
    let region = match state.region_table.lookup(region_id) {
        Some(r) => r,
        None => return Json(json!({"error": "region not found"})),
    };

    let offset = region.base_offset + va.offset() + req.offset;

    match region.state {
        RegionState::Home => {
            // Write to local MR
            if let Some(entry) = state.mr_table.lookup(region.mr_handle) {
                match entry.device.write(offset, &req.data) {
                    Ok(()) => Json(json!({"status": "ok"})),
                    Err(e) => Json(json!({"error": format!("{}", e)})),
                }
            } else {
                Json(json!({"error": "home MR not found"}))
            }
        }
        RegionState::Shared(_) | RegionState::Invalid => {
            // Write remotely to home node
            write_va_remote(&state, &region, offset, &req.data).await
        }
    }
}

async fn acquire_writer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AcquireWriterRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let va = match parse_va(&req.va) {
        Ok(v) => v,
        Err(e) => return Json(json!({"error": e})),
    };

    let region_id = va.region_id();

    // Try to acquire writer via CoherenceManager
    let result = state.coherence.acquire_writer(region_id, state.config.node_id);
    match result {
        ub_managed::AcquireWriterResult::Granted { epoch, .. } => {
            Json(json!({
                "status": "granted",
                "va": format!("{:032x}", va.0),
                "region_id": region_id.0,
                "epoch": epoch,
            }))
        }
        ub_managed::AcquireWriterResult::Denied { reason, .. } => {
            Json(json!({"status": "denied", "reason": reason}))
        }
        ub_managed::AcquireWriterResult::Queued { .. } => {
            Json(json!({"status": "queued"}))
        }
        ub_managed::AcquireWriterResult::NotFound => {
            Json(json!({"error": "region not found in coherence manager"}))
        }
    }
}

async fn release_writer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReleaseWriterRequest>,
) -> Json<Value> {
    if !state.config.managed.enabled {
        return Json(json!({"error": "managed layer not enabled"}));
    }

    let va = match parse_va(&req.va) {
        Ok(v) => v,
        Err(e) => return Json(json!({"error": e})),
    };

    let region_id = va.region_id();

    let released = state.coherence.release_writer(region_id, state.config.node_id);
    if released {
        let epoch = state.coherence.get_epoch(region_id).unwrap_or(0);
        Json(json!({
            "status": "ok",
            "region_id": region_id.0,
            "epoch": epoch,
        }))
    } else {
        Json(json!({"error": "failed to release writer (not the current writer or region not found)"}))
    }
}

/// Write to a remote region's home node via data plane.
async fn write_va_remote(state: &Arc<AppState>, region: &RegionInfo, offset: u64, data: &[u8]) -> Json<Value> {
    let ub_addr = UbAddr::new(
        state.config.pod_id,
        region.home_node_id,
        region.device_id,
        offset,
        0,
    );
    let dp = state.dataplane.lock().await;
    match dp.ub_write_remote(ub_addr, data).await {
        Ok(()) => Json(json!({"status": "ok"})),
        Err(e) => Json(json!({"error": format!("{}", e)})),
    }
}

/// Parse a hex-encoded u128 virtual address.
fn parse_va(s: &str) -> Result<UbVa, String> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u128::from_str_radix(s, 16)
        .map(UbVa)
        .map_err(|e| format!("invalid virtual address: {e}"))
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

    // Region count gauges by state
    let (home, shared, invalid) = state.region_table.count_by_state();
    ub_obs::set_gauge_label(ub_obs::REGION_COUNT, "state", "home", home as f64);
    ub_obs::set_gauge_label(ub_obs::REGION_COUNT, "state", "shared", shared as f64);
    ub_obs::set_gauge_label(ub_obs::REGION_COUNT, "state", "invalid", invalid as f64);

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
