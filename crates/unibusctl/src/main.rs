use std::time::Instant;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "unibusctl", about = "UniBus CLI")]
struct Cli {
    /// Address of the unibusd admin API
    #[arg(short, long, default_value = "http://127.0.0.1:9090")]
    addr: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new unibusd node process
    NodeStart {
        /// Path to the node configuration YAML file
        #[arg(long)]
        config: String,
    },
    /// List all nodes in the SuperPod
    NodeList,
    /// Show info about a specific node
    NodeInfo,
    /// List all local MRs
    MrList,
    /// List all remote MR cache entries
    MrCache,
    /// Create a new Jetty
    JettyCreate,
    /// List all local Jettys
    JettyList,
    /// Post a receive buffer to a Jetty
    JettyPostRecv {
        /// Jetty handle
        jetty_handle: u32,
        /// Buffer length in bytes
        len: u32,
    },
    /// Poll a completion queue entry from a Jetty
    JettyPollCqe {
        /// Jetty handle
        jetty_handle: u32,
    },
    /// Run a quick latency/throughput benchmark via admin API
    Bench {
        /// UB address of the target MR
        #[arg(long)]
        ub_addr: String,
        /// Number of iterations per verb
        #[arg(short, long, default_value = "100")]
        iterations: u32,
        /// Data size in bytes for write/read
        #[arg(short, long, default_value = "1024")]
        size: u32,
    },
    /// Initialize a KV store on the target node
    KvInit {
        /// Number of KV slots
        #[arg(short, long, default_value = "16")]
        slots: u32,
        /// Device kind: "memory" or "npu"
        #[arg(long, default_value = "memory")]
        device_kind: String,
    },
    /// Put a key-value pair into a KV slot
    KvPut {
        /// UB address of the KV MR
        ub_addr: String,
        /// Slot index
        slot: u32,
        /// Key string
        key: String,
        /// Value string
        value: String,
        /// Replica node ID for write_with_imm notification
        #[arg(long)]
        replica_node_id: Option<u16>,
        /// Replica jetty ID for notification
        #[arg(long)]
        replica_jetty_id: Option<u32>,
    },
    /// Get a value from a KV slot
    KvGet {
        /// UB address of the KV MR
        ub_addr: String,
        /// Slot index
        slot: u32,
        /// Key string
        key: String,
    },
    /// Compare-and-swap a KV slot
    KvCas {
        /// UB address of the KV MR
        ub_addr: String,
        /// Slot index
        slot: u32,
        /// Key string
        key: String,
        /// Expected current version
        expect_version: u64,
        /// New value string
        value: String,
        /// Replica node ID for write_with_imm notification on success
        #[arg(long)]
        replica_node_id: Option<u16>,
        /// Replica jetty ID for notification
        #[arg(long)]
        replica_jetty_id: Option<u32>,
    },
    /// List all registered device profiles (Managed layer)
    DeviceList,
    /// Allocate a region in the managed layer (ub_alloc)
    RegionAlloc {
        /// Size in bytes to allocate
        #[arg(short, long)]
        size: u64,
        /// Latency class: "critical", "normal", "bulk"
        #[arg(long, default_value = "normal")]
        latency_class: String,
        /// Capacity class: "small", "large", "huge"
        #[arg(long, default_value = "small")]
        capacity_class: String,
        /// Pin to device kind: "memory" or "npu"
        #[arg(long)]
        pin: Option<String>,
    },
    /// Free a region in the managed layer (ub_free)
    RegionFree {
        /// Region ID to free
        region_id: u64,
    },
    /// List all regions in the managed layer
    RegionList,
    /// Read from a virtual address (ub_read_va)
    VaRead {
        /// Virtual address (hex u128)
        va: String,
        /// Number of bytes to read
        #[arg(short, long, default_value = "64")]
        len: u32,
        /// Offset within the region
        #[arg(long, default_value = "0")]
        offset: u64,
    },
    /// Write to a virtual address (ub_write_va)
    VaWrite {
        /// Virtual address (hex u128)
        va: String,
        /// Data string to write
        data: String,
        /// Offset within the region
        #[arg(long, default_value = "0")]
        offset: u64,
    },
    /// Acquire writer lock for a region (SWMR)
    AcquireWriter {
        /// Virtual address (hex u128)
        va: String,
    },
    /// Release writer lock for a region (SWMR)
    ReleaseWriter {
        /// Virtual address (hex u128)
        va: String,
    },
    /// Register a remote region in the local region table (for cross-node discovery)
    RegionRegisterRemote {
        /// Region ID
        region_id: u64,
        /// Home node ID
        home_node_id: u16,
        /// Device ID on the home node
        device_id: u16,
        /// MR handle on the home node
        mr_handle: u32,
        /// Base offset within the home MR
        #[arg(long, default_value = "0")]
        base_offset: u64,
        /// Region length in bytes
        len: u64,
        /// Current epoch
        #[arg(long, default_value = "0")]
        epoch: u64,
    },
    /// Invalidate a cached region on this node (simulates INVALIDATE from home node)
    RegionInvalidate {
        /// Region ID to invalidate
        region_id: u64,
        /// New epoch from the writer
        #[arg(long, default_value = "0")]
        new_epoch: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Commands::NodeStart { config } => {
            // Spawn unibusd as a child process
            let child = std::process::Command::new("unibusd")
                .args(["--config", &config])
                .spawn();

            match child {
                Ok(child) => {
                    let pid = child.id();
                    println!("unibusd started: PID={pid}, config={config}");
                }
                Err(e) => {
                    eprintln!("Failed to start unibusd: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::NodeList => {
            let url = format!("{}/admin/node/list", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            if let Some(nodes) = body.get("nodes").and_then(|n| n.as_array()) {
                if nodes.is_empty() {
                    println!("No nodes found.");
                } else {
                    println!("{:<10} {:<12} {:<25} {:<8} {:<10}", "NodeID", "State", "DataAddr", "Epoch", "Credits");
                    for node in nodes {
                        let id = node.get("node_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let state = node.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                        let addr = node.get("data_addr").and_then(|v| v.as_str()).unwrap_or("?");
                        let epoch = node.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                        let credits = node.get("initial_credits").and_then(|v| v.as_u64()).unwrap_or(0);
                        println!("{:<10} {:<12} {:<25} {:<8} {:<10}", id, state, addr, epoch, credits);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::NodeInfo => {
            let url = format!("{}/admin/node/info", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{body}");
        }
        Commands::MrList => {
            let url = format!("{}/admin/mr/list", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            if let Some(mrs) = body.get("mrs").and_then(|n| n.as_array()) {
                if mrs.is_empty() {
                    println!("No MRs registered.");
                } else {
                    println!("{:<10} {:<12} {:<40} {:<10} {:<12} {:<10}", "Handle", "DeviceKind", "UbAddr", "Len", "Perms", "State");
                    for mr in mrs {
                        let handle = mr.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
                        let kind = mr.get("device_kind").and_then(|v| v.as_str()).unwrap_or("?");
                        let addr = mr.get("ub_addr").and_then(|v| v.as_str()).unwrap_or("?");
                        let len = mr.get("len").and_then(|v| v.as_u64()).unwrap_or(0);
                        let perms = mr.get("perms").and_then(|v| v.as_str()).unwrap_or("?");
                        let state = mr.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                        println!("{:<10} {:<12} {:<40} {:<10} {:<12} {:<10}", handle, kind, addr, len, perms, state);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::MrCache => {
            let url = format!("{}/admin/mr/cache", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            if let Some(cache) = body.get("cache").and_then(|n| n.as_array()) {
                if cache.is_empty() {
                    println!("No remote MRs cached.");
                } else {
                    println!("{:<12} {:<10} {:<40} {:<10} {:<12}", "OwnerNode", "Handle", "UbAddr", "Len", "DeviceKind");
                    for entry in cache {
                        let owner = entry.get("owner_node").and_then(|v| v.as_u64()).unwrap_or(0);
                        let handle = entry.get("mr_handle").and_then(|v| v.as_u64()).unwrap_or(0);
                        let addr = entry.get("ub_addr").and_then(|v| v.as_str()).unwrap_or("?");
                        let len = entry.get("len").and_then(|v| v.as_u64()).unwrap_or(0);
                        let kind = entry.get("device_kind").and_then(|v| v.as_str()).unwrap_or("?");
                        println!("{:<12} {:<10} {:<40} {:<10} {:<12}", owner, handle, addr, len, kind);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::JettyCreate => {
            let url = format!("{}/admin/jetty/create", cli.addr);
            let resp = client.post(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;
            if let Some(handle) = body.get("jetty_handle").and_then(|v| v.as_u64()) {
                println!("Jetty created: handle={handle}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::JettyList => {
            let url = format!("{}/admin/jetty/list", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            if let Some(jettys) = body.get("jettys").and_then(|n| n.as_array()) {
                if jettys.is_empty() {
                    println!("No Jettys created.");
                } else {
                    println!("{:<10} {:<10} {:<10} {:<10} {:<10} {:<15} {:<10} {:<10} {:<10}",
                        "Handle", "NodeID", "JFSDepth", "JFRDepth", "JFCDepth", "JFCHWM", "CQECount", "JFSCount", "JFRCount");
                    for j in jettys {
                        let handle = j.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
                        let node_id = j.get("node_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfs_depth = j.get("jfs_depth").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfr_depth = j.get("jfr_depth").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfc_depth = j.get("jfc_depth").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfc_hwm = j.get("jfc_high_watermark").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cqe_count = j.get("cqe_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfs_count = j.get("jfs_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let jfr_count = j.get("jfr_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        println!("{:<10} {:<10} {:<10} {:<10} {:<10} {:<15} {:<10} {:<10} {:<10}",
                            handle, node_id, jfs_depth, jfr_depth, jfc_depth, jfc_hwm, cqe_count, jfs_count, jfr_count);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::JettyPostRecv { jetty_handle, len } => {
            let url = format!("{}/admin/jetty/post-recv", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"jetty_handle": jetty_handle, "len": len}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                println!("Recv buffer posted to jetty {jetty_handle} (len={len})");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::JettyPollCqe { jetty_handle } => {
            let url = format!("{}/admin/jetty/poll-cqe", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"jetty_handle": jetty_handle}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("cqe").and_then(|v| v.as_null()).is_some() {
                println!("No CQE available.");
            } else if body.get("wr_id").is_some() {
                let wr_id = body.get("wr_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let status = body.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
                let imm = body.get("imm");
                let byte_len = body.get("byte_len").and_then(|v| v.as_u64()).unwrap_or(0);
                let verb = body.get("verb").and_then(|v| v.as_str()).unwrap_or("?");
                println!("CQE: wr_id={wr_id}, status={status}, imm={imm:?}, byte_len={byte_len}, verb={verb}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::Bench { ub_addr, iterations, size } => {
            println!("UniBus Verb Benchmark");
            println!("  target: {}", ub_addr);
            println!("  iterations: {}", iterations);
            println!("  data size: {} bytes", size);

            let data = vec![0xABu8; size as usize];

            // Write benchmark
            let mut write_latencies = Vec::with_capacity(iterations as usize);
            let mut ok_count = 0u32;
            let mut err_count = 0u32;
            for _ in 0..iterations {
                let start = Instant::now();
                let url = format!("{}/admin/verb/write", cli.addr);
                let resp = client.post(&url)
                    .json(&serde_json::json!({"ub_addr": ub_addr, "data": data}))
                    .send().await?;
                let body: serde_json::Value = resp.json().await?;
                let elapsed = start.elapsed();
                if body.get("status").is_some() {
                    ok_count += 1;
                } else {
                    err_count += 1;
                }
                write_latencies.push(elapsed.as_micros() as f64);
            }
            print_stats("write", &write_latencies, ok_count, err_count, size);

            // Read benchmark
            let mut read_latencies = Vec::with_capacity(iterations as usize);
            let mut ok_count = 0u32;
            let mut err_count = 0u32;
            for _ in 0..iterations {
                let start = Instant::now();
                let url = format!("{}/admin/verb/read", cli.addr);
                let resp = client.post(&url)
                    .json(&serde_json::json!({"ub_addr": ub_addr, "len": size}))
                    .send().await?;
                let body: serde_json::Value = resp.json().await?;
                let elapsed = start.elapsed();
                if body.get("data").is_some() {
                    ok_count += 1;
                } else {
                    err_count += 1;
                }
                read_latencies.push(elapsed.as_micros() as f64);
            }
            print_stats("read", &read_latencies, ok_count, err_count, size);

            // Atomic FAA benchmark
            let mut faa_latencies = Vec::with_capacity(iterations as usize);
            let mut ok_count = 0u32;
            let mut err_count = 0u32;
            for _ in 0..iterations {
                let start = Instant::now();
                let url = format!("{}/admin/verb/atomic-faa", cli.addr);
                let resp = client.post(&url)
                    .json(&serde_json::json!({"ub_addr": ub_addr, "add": 1}))
                    .send().await?;
                let body: serde_json::Value = resp.json().await?;
                let elapsed = start.elapsed();
                if body.get("old_value").is_some() {
                    ok_count += 1;
                } else {
                    err_count += 1;
                }
                faa_latencies.push(elapsed.as_micros() as f64);
            }
            print_stats("atomic_faa", &faa_latencies, ok_count, err_count, 8);
        }
        Commands::KvInit { slots, device_kind } => {
            let url = format!("{}/admin/kv/init", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"slots": slots, "device_kind": device_kind}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                let addr = body.get("ub_addr").and_then(|v| v.as_str()).unwrap_or("?");
                let handle = body.get("mr_handle").and_then(|v| v.as_u64()).unwrap_or(0);
                let slot_size = body.get("slot_size").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("KV store initialized: slots={slots} handle={handle} addr={addr} slot_size={slot_size}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::KvPut { ub_addr, slot, key, value, replica_node_id, replica_jetty_id } => {
            let url = format!("{}/admin/kv/put", cli.addr);
            let mut body = serde_json::json!({"ub_addr": ub_addr, "slot": slot, "key": key, "value": value});
            if let Some(node_id) = replica_node_id {
                body["replica_node_id"] = serde_json::json!(node_id);
            }
            if let Some(jetty_id) = replica_jetty_id {
                body["replica_jetty_id"] = serde_json::json!(jetty_id);
            }
            let resp = client.post(&url)
                .json(&body)
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                let version = body.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("PUT slot={slot} key={key} version={version}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::KvGet { ub_addr, slot, key } => {
            let url = format!("{}/admin/kv/get", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"ub_addr": ub_addr, "slot": slot, "key": key}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("key").is_some() {
                let found_key = body.get("key").and_then(|v| v.as_str()).unwrap_or("?");
                let found_value = body.get("value").and_then(|v| v.as_str()).unwrap_or("?");
                let version = body.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("GET slot={slot} key={found_key} value={found_value} version={version}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::KvCas { ub_addr, slot, key, expect_version, value, replica_node_id, replica_jetty_id } => {
            let url = format!("{}/admin/kv/cas", cli.addr);
            let mut body = serde_json::json!({"ub_addr": ub_addr, "slot": slot, "key": key, "expect_version": expect_version, "value": value});
            if let Some(node_id) = replica_node_id {
                body["replica_node_id"] = serde_json::json!(node_id);
            }
            if let Some(jetty_id) = replica_jetty_id {
                body["replica_jetty_id"] = serde_json::json!(jetty_id);
            }
            let resp = client.post(&url)
                .json(&body)
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").and_then(|v| v.as_str()) == Some("ok") {
                let new_ver = body.get("new_version").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("CAS slot={slot} key={key} succeeded: version {expect_version} -> {new_ver}");
            } else if body.get("status").and_then(|v| v.as_str()) == Some("cas_failed") {
                let current = body.get("current_version").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("CAS slot={slot} key={key} failed: expected version {expect_version}, current is {current}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::DeviceList => {
            let url = format!("{}/admin/device/list", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;

            if let Some(devices) = body.get("devices").and_then(|n| n.as_array()) {
                if devices.is_empty() {
                    println!("No devices registered.");
                } else {
                    println!("{:<8} {:<10} {:<8} {:<6} {:<14} {:<10} {:<10} {:<10} {:<16} {:<16} {:<14} {:<14} {:<10}",
                        "NodeID", "DevID", "Kind", "Tier", "Capacity", "Used", "Free", "FreeRatio", "ReadBW(Mbps)", "WriteBW(Mbps)", "ReadLat(ns)", "WriteLat(ns)", "RPS");
                    for d in devices {
                        let node_id = d.get("node_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let dev_id = d.get("device_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let kind = d.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                        let tier = d.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
                        let cap = d.get("capacity_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let used = d.get("used_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let free = d.get("free_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let free_ratio = d.get("free_ratio").and_then(|v| v.as_str()).unwrap_or("?");
                        let r_bw = d.get("peak_read_bw_mbps").and_then(|v| v.as_u64()).unwrap_or(0);
                        let w_bw = d.get("peak_write_bw_mbps").and_then(|v| v.as_u64()).unwrap_or(0);
                        let r_lat = d.get("read_latency_ns_p50").and_then(|v| v.as_u64()).unwrap_or(0);
                        let w_lat = d.get("write_latency_ns_p50").and_then(|v| v.as_u64()).unwrap_or(0);
                        let rps = d.get("recent_rps").and_then(|v| v.as_u64()).unwrap_or(0);
                        println!("{:<8} {:<10} {:<8} {:<6} {:<14} {:<10} {:<10} {:<10} {:<16} {:<16} {:<14} {:<14} {:<10}",
                            node_id, dev_id, kind, tier, cap, used, free, free_ratio, r_bw, w_bw, r_lat, w_lat, rps);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::RegionAlloc { size, latency_class, capacity_class, pin } => {
            let url = format!("{}/admin/region/alloc", cli.addr);
            let mut body = serde_json::json!({
                "size": size,
                "latency_class": latency_class,
                "capacity_class": capacity_class,
            });
            if let Some(ref p) = pin {
                body["pin"] = serde_json::json!(p);
            }
            let resp = client.post(&url).json(&body).send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                let va = body.get("va").and_then(|v| v.as_str()).unwrap_or("?");
                let region_id = body.get("region_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let home = body.get("home_node_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let len = body.get("len").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("Region allocated: region_id={region_id} va=0x{va} home_node={home} len={len}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::RegionFree { region_id } => {
            let url = format!("{}/admin/region/free", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"region_id": region_id}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                println!("Region {region_id} freed");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::RegionList => {
            let url = format!("{}/admin/region/list", cli.addr);
            let resp = client.get(&url).send().await?;
            let body: serde_json::Value = resp.json().await?;
            if let Some(regions) = body.get("regions").and_then(|n| n.as_array()) {
                if regions.is_empty() {
                    println!("No regions allocated.");
                } else {
                    println!("{:<12} {:<12} {:<10} {:<10} {:<12} {:<10} {:<8} {:<10} {:<14} {:<8} {:<12}",
                        "RegionID", "HomeNode", "DeviceID", "MRHandle", "BaseOffset", "Len", "Epoch", "State", "LocalMR", "Writer", "Readers");
                    for r in regions {
                        let rid = r.get("region_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let home = r.get("home_node_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let dev = r.get("device_id").and_then(|v| v.as_u64()).unwrap_or(0);
                        let mr = r.get("mr_handle").and_then(|v| v.as_u64()).unwrap_or(0);
                        let base = r.get("base_offset").and_then(|v| v.as_u64()).unwrap_or(0);
                        let len = r.get("len").and_then(|v| v.as_u64()).unwrap_or(0);
                        let epoch = r.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                        let state = r.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                        let local_mr = r.get("local_mr_handle").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or("-".to_string());
                        let writer = r.get("writer_node_id").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or("-".to_string());
                        let readers = r.get("readers").and_then(|v| v.as_array()).map(|arr| {
                            arr.iter().filter_map(|v| v.as_u64().map(|n| n.to_string())).collect::<Vec<_>>().join(",")
                        }).unwrap_or_else(|| "-".to_string());
                        println!("{:<12} {:<12} {:<10} {:<10} {:<12} {:<10} {:<8} {:<10} {:<14} {:<8} {:<12}",
                            rid, home, dev, mr, base, len, epoch, state, local_mr, writer, readers);
                    }
                }
            } else {
                println!("Unexpected response: {body}");
            }
        }
        Commands::VaRead { va, len, offset } => {
            let url = format!("{}/admin/verb/read-va", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"va": va, "offset": offset, "len": len}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("data").is_some() {
                let data = body.get("data").and_then(|v| v.as_array()).unwrap();
                let bytes: Vec<u8> = data.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect();
                let data_len = body.get("len").and_then(|v| v.as_u64()).unwrap_or(bytes.len() as u64);
                let text = String::from_utf8_lossy(&bytes);
                println!("Read {} bytes from va={va} offset={offset}: {:?}", data_len, text);
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::VaWrite { va, data, offset } => {
            let url = format!("{}/admin/verb/write-va", cli.addr);
            let data_bytes = data.as_bytes().to_vec();
            let resp = client.post(&url)
                .json(&serde_json::json!({"va": va, "offset": offset, "data": data_bytes}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                println!("Write {} bytes to va={va} offset={offset}", data.len());
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::AcquireWriter { va } => {
            let url = format!("{}/admin/verb/acquire-writer", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"va": va}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            if status == "granted" {
                let region_id = body.get("region_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let epoch = body.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("Writer lock acquired: va={va} region_id={region_id} epoch={epoch}");
            } else if status == "denied" {
                let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown");
                println!("Writer lock denied: {reason}");
            } else if status == "queued" {
                println!("Writer lock queued (waiting for current writer to release)");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::ReleaseWriter { va } => {
            let url = format!("{}/admin/verb/release-writer", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"va": va}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                let region_id = body.get("region_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let epoch = body.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("Writer lock released: region_id={region_id} epoch={epoch}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::RegionRegisterRemote { region_id, home_node_id, device_id, mr_handle, base_offset, len, epoch } => {
            let url = format!("{}/admin/region/register-remote", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({
                    "region_id": region_id,
                    "home_node_id": home_node_id,
                    "device_id": device_id,
                    "mr_handle": mr_handle,
                    "base_offset": base_offset,
                    "len": len,
                    "epoch": epoch,
                }))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            if body.get("status").is_some() {
                println!("Remote region registered: region_id={region_id} home_node={home_node_id}");
            } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                println!("Error: {err}");
            } else {
                println!("{body}");
            }
        }
        Commands::RegionInvalidate { region_id, new_epoch } => {
            let url = format!("{}/admin/region/invalidate", cli.addr);
            let resp = client.post(&url)
                .json(&serde_json::json!({"region_id": region_id, "new_epoch": new_epoch}))
                .send().await?;
            let body: serde_json::Value = resp.json().await?;
            let invalidated = body.get("invalidated").and_then(|v| v.as_bool()).unwrap_or(false);
            if invalidated {
                println!("Region {region_id} invalidated (new_epoch={new_epoch})");
            } else {
                println!("Region {region_id} not invalidated (not cached or stale)");
            }
        }
    }

    Ok(())
}

fn print_stats(verb: &str, latencies: &[f64], ok: u32, err: u32, size_bytes: u32) {
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let n = sorted.len();
    let p50 = sorted[n / 2];
    let p90 = sorted[(n as f64 * 0.9) as usize];
    let p99 = sorted[(n as f64 * 0.99) as usize];
    let avg: f64 = sorted.iter().sum::<f64>() / n as f64;
    let total_time_s: f64 = sorted.iter().sum::<f64>() / 1_000_000.0;
    let ops_per_s = if total_time_s > 0.0 { ok as f64 / total_time_s } else { 0.0 };
    let throughput_mib = if total_time_s > 0.0 {
        (ok as f64 * size_bytes as f64) / (1024.0 * 1024.0) / total_time_s
    } else {
        0.0
    };

    println!("\n  {}:", verb);
    println!("    ok={ok} err={err}");
    println!("    latency (µs): avg={avg:.1} p50={p50:.1} p90={p90:.1} p99={p99:.1}");
    println!("    throughput:    {ops_per_s:.0} ops/s  {throughput_mib:.2} MiB/s");
}
