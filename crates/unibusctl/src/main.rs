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
    }

    Ok(())
}
