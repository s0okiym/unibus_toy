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
