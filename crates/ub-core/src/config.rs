use serde::{Deserialize, Serialize};

/// Top-level node configuration (§11).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    #[serde(default = "default_pod_id")]
    pub pod_id: u16,

    pub node_id: u16,

    #[serde(default)]
    pub control: ControlConfig,

    #[serde(default)]
    pub data: DataConfig,

    #[serde(default)]
    pub mr: MrConfig,

    #[serde(default)]
    pub device: DeviceConfig,

    #[serde(default)]
    pub transport: TransportConfig,

    #[serde(default)]
    pub flow: FlowConfig,

    #[serde(default)]
    pub jetty: JettyConfig,

    #[serde(default)]
    pub obs: ObsConfig,

    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    #[serde(default)]
    pub managed: ManagedConfig,
}

fn default_pod_id() -> u16 {
    1
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            pod_id: 1,
            node_id: 0,
            control: ControlConfig::default(),
            data: DataConfig::default(),
            mr: MrConfig::default(),
            device: DeviceConfig::default(),
            transport: TransportConfig::default(),
            flow: FlowConfig::default(),
            jetty: JettyConfig::default(),
            obs: ObsConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            managed: ManagedConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    #[serde(default = "default_control_listen")]
    pub listen: String,

    #[serde(default = "default_bootstrap")]
    pub bootstrap: String,

    #[serde(default)]
    pub peers: Vec<String>,

    #[serde(default)]
    pub seed_addrs: Vec<String>,

    #[serde(default)]
    pub hub_node_id: u16,
}

fn default_control_listen() -> String {
    "0.0.0.0:7900".to_string()
}

fn default_bootstrap() -> String {
    "static".to_string()
}

impl Default for ControlConfig {
    fn default() -> Self {
        ControlConfig {
            listen: default_control_listen(),
            bootstrap: default_bootstrap(),
            peers: Vec::new(),
            seed_addrs: Vec::new(),
            hub_node_id: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataConfig {
    #[serde(default = "default_data_listen")]
    pub listen: String,

    #[serde(default = "default_fabric")]
    pub fabric: String,

    #[serde(default = "default_mtu")]
    pub mtu: u16,
}

fn default_data_listen() -> String {
    "0.0.0.0:7901".to_string()
}

fn default_fabric() -> String {
    "udp".to_string()
}

fn default_mtu() -> u16 {
    1400
}

impl Default for DataConfig {
    fn default() -> Self {
        DataConfig {
            listen: default_data_listen(),
            fabric: default_fabric(),
            mtu: default_mtu(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MrConfig {
    #[serde(default = "default_deregister_timeout")]
    pub deregister_timeout_ms: u64,
}

fn default_deregister_timeout() -> u64 {
    500
}

impl Default for MrConfig {
    fn default() -> Self {
        MrConfig {
            deregister_timeout_ms: default_deregister_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    #[serde(default)]
    pub npu: NpuConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpuConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_npu_mem")]
    pub mem_size_mib: u64,
}

fn default_true() -> bool {
    true
}

fn default_npu_mem() -> u64 {
    256
}

impl Default for NpuConfig {
    fn default() -> Self {
        NpuConfig {
            enabled: true,
            mem_size_mib: 256,
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig {
            npu: NpuConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportConfig {
    #[serde(default = "default_rto")]
    pub rto_ms: u64,

    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    #[serde(default = "default_sack_bits")]
    pub sack_bitmap_bits: u16,

    #[serde(default = "default_reassembly_budget")]
    pub reassembly_budget_bytes: u64,
}

fn default_rto() -> u64 {
    200
}

fn default_max_retries() -> u32 {
    8
}

fn default_sack_bits() -> u16 {
    256
}

fn default_reassembly_budget() -> u64 {
    67108864 // 64 MiB
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            rto_ms: default_rto(),
            max_retries: default_max_retries(),
            sack_bitmap_bits: default_sack_bits(),
            reassembly_budget_bytes: default_reassembly_budget(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowConfig {
    #[serde(default = "default_initial_credits")]
    pub initial_credits: u32,
}

fn default_initial_credits() -> u32 {
    64
}

impl Default for FlowConfig {
    fn default() -> Self {
        FlowConfig {
            initial_credits: default_initial_credits(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JettyConfig {
    #[serde(default = "default_jfs_depth")]
    pub jfs_depth: u32,

    #[serde(default = "default_jfr_depth")]
    pub jfr_depth: u32,

    #[serde(default = "default_jfc_depth")]
    pub jfc_depth: u32,

    #[serde(default = "default_jfc_high_watermark")]
    pub jfc_high_watermark: u32,
}

fn default_jfs_depth() -> u32 {
    1024
}

fn default_jfr_depth() -> u32 {
    1024
}

fn default_jfc_depth() -> u32 {
    1024
}

fn default_jfc_high_watermark() -> u32 {
    896
}

impl Default for JettyConfig {
    fn default() -> Self {
        JettyConfig {
            jfs_depth: default_jfs_depth(),
            jfr_depth: default_jfr_depth(),
            jfc_depth: default_jfc_depth(),
            jfc_high_watermark: default_jfc_high_watermark(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObsConfig {
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: String,
}

fn default_metrics_listen() -> String {
    "127.0.0.1:9090".to_string()
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            metrics_listen: default_metrics_listen(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    #[serde(default = "default_heartbeat_interval")]
    pub interval_ms: u64,

    #[serde(default = "default_fail_after")]
    pub fail_after: u32,
}

fn default_heartbeat_interval() -> u64 {
    1000
}

fn default_fail_after() -> u32 {
    3
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        HeartbeatConfig {
            interval_ms: default_heartbeat_interval(),
            fail_after: default_fail_after(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub placer_node_id: u16,

    #[serde(default)]
    pub pool: PoolConfig,

    #[serde(default = "default_writer_lease_ms")]
    pub writer_lease_ms: u64,

    #[serde(default = "default_true_val")]
    pub fetch_block_on_writer: bool,

    #[serde(default)]
    pub cache: CacheConfig,

    #[serde(default)]
    pub cost_weights: CostWeights,
}

fn default_memory_reserve_ratio() -> f64 {
    0.25
}

fn default_writer_lease_ms() -> u64 {
    5000
}

fn default_true_val() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    #[serde(default = "default_memory_reserve_ratio")]
    pub memory_reserve_ratio: f64,

    #[serde(default = "default_npu_reserve_ratio")]
    pub npu_reserve_ratio: f64,
}

fn default_npu_reserve_ratio() -> f64 {
    0.50
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            memory_reserve_ratio: default_memory_reserve_ratio(),
            npu_reserve_ratio: default_npu_reserve_ratio(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_max_bytes")]
    pub max_bytes: u64,

    #[serde(default = "default_eviction")]
    pub eviction: String,
}

fn default_cache_max_bytes() -> u64 {
    536870912 // 512 MiB
}

fn default_eviction() -> String {
    "lru".to_string()
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            max_bytes: default_cache_max_bytes(),
            eviction: default_eviction(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostWeights {
    #[serde(default = "default_weight_1")]
    pub latency: f64,

    #[serde(default = "default_weight_03")]
    pub capacity: f64,

    #[serde(default = "default_weight_05")]
    pub tier_match: f64,

    #[serde(default = "default_weight_02")]
    pub load: f64,
}

fn default_weight_1() -> f64 { 1.0 }
fn default_weight_03() -> f64 { 0.3 }
fn default_weight_05() -> f64 { 0.5 }
fn default_weight_02() -> f64 { 0.2 }

impl Default for CostWeights {
    fn default() -> Self {
        CostWeights {
            latency: default_weight_1(),
            capacity: default_weight_03(),
            tier_match: default_weight_05(),
            load: default_weight_02(),
        }
    }
}

impl Default for ManagedConfig {
    fn default() -> Self {
        ManagedConfig {
            enabled: false,
            placer_node_id: 0,
            pool: PoolConfig::default(),
            writer_lease_ms: default_writer_lease_ms(),
            fetch_block_on_writer: true,
            cache: CacheConfig::default(),
            cost_weights: CostWeights::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_roundtrip() {
        let config = NodeConfig {
            pod_id: 1,
            node_id: 42,
            control: ControlConfig {
                listen: "0.0.0.0:7900".to_string(),
                bootstrap: "static".to_string(),
                peers: vec!["10.0.0.1:7900".to_string()],
                seed_addrs: vec![],
                hub_node_id: 0,
            },
            data: DataConfig::default(),
            mr: MrConfig::default(),
            device: DeviceConfig::default(),
            transport: TransportConfig::default(),
            flow: FlowConfig::default(),
            jetty: JettyConfig::default(),
            obs: ObsConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            managed: ManagedConfig::default(),
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: NodeConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.pod_id, 1);
        assert_eq!(parsed.node_id, 42);
        assert_eq!(parsed.control.bootstrap, "static");
        assert_eq!(parsed.control.peers.len(), 1);
    }

    #[test]
    fn test_config_defaults() {
        let yaml = "node_id: 1\n";
        let config: NodeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.pod_id, 1);
        assert_eq!(config.data.mtu, 1400);
        assert_eq!(config.transport.rto_ms, 200);
        assert_eq!(config.flow.initial_credits, 64);
        assert_eq!(config.heartbeat.interval_ms, 1000);
        assert_eq!(config.heartbeat.fail_after, 3);
        assert!(!config.managed.enabled);
    }

    #[test]
    fn test_config_missing_node_id() {
        let yaml = "pod_id: 1\n";
        let result = serde_yaml::from_str::<NodeConfig>(yaml);
        // node_id is required (no default), should fail
        assert!(result.is_err());
    }
}
