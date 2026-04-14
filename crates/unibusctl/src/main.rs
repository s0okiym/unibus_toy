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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
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
    }

    Ok(())
}
