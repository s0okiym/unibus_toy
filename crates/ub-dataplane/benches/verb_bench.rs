//! Criterion benchmarks for UniBus verbs over local loopback UDP.
//!
//! Measures throughput and latency for write/read/atomic operations
//! between two DataPlaneEngine instances connected via loopback.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tokio::sync::mpsc;

use ub_core::addr::UbAddr;
use ub_core::config::{FlowConfig, JettyConfig, TransportConfig};
use ub_core::device::memory::MemoryDevice;
use ub_core::device::Device;
use ub_core::mr::{MrCacheTable, MrTable};
use ub_core::types::{DeviceKind, MrPerms};
use ub_dataplane::DataPlaneEngine;
use ub_fabric::udp::UdpFabric;
use ub_transport::manager::TransportManager;

fn transport_config() -> (TransportConfig, FlowConfig) {
    (
        TransportConfig {
            rto_ms: 200,
            max_retries: 8,
            sack_bitmap_bits: 256,
            reassembly_budget_bytes: 67108864,
        },
        FlowConfig {
            initial_credits: 1024,
        },
    )
}

/// Create a DataPlaneEngine with transport and start it.
fn make_engine(node_id: u16, fabric: Arc<UdpFabric>) -> DataPlaneEngine {
    let (mr_table, mr_event_rx) = MrTable::new_with_channel(1, node_id);
    let mr_table = Arc::new(mr_table);
    let mr_cache = Arc::new(MrCacheTable::new());
    let jetty_table = Arc::new(ub_core::jetty::JettyTable::new(node_id, JettyConfig::default()));

    let (tconfig, fconfig) = transport_config();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let mut transport = TransportManager::new(node_id, tconfig, fconfig, inbound_tx);
    transport.start_rto_task();
    let transport = Arc::new(transport);

    let _ = mr_event_rx; // not needed in benchmarks
    DataPlaneEngine::new(node_id, mr_table, mr_cache, jetty_table, fabric, transport, inbound_rx)
}

/// Set up two connected engines with an MR registered on node A.
async fn setup_two_nodes() -> (DataPlaneEngine, DataPlaneEngine, UbAddr) {
    let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
    let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

    let mut engine_a = make_engine(1, fabric_a);
    let mut engine_b = make_engine(2, fabric_b);

    engine_a.start().await.unwrap();
    engine_b.start().await.unwrap();

    // Connect bidirectionally
    let addr_a = engine_a.fabric().local_addr();
    let addr_b = engine_b.fabric().local_addr();
    engine_a.connect_peer(2, addr_b).await.unwrap();
    engine_b.connect_peer(1, addr_a).await.unwrap();

    // Create transport sessions
    engine_a.transport().create_session(2, 1, 1024).unwrap();
    engine_b.transport().create_session(1, 1, 1024).unwrap();

    // Register MR on node A
    let dev: Arc<dyn Device> = Arc::new(MemoryDevice::new(65536));
    let (mr_addr, _handle) = engine_a.mr_table().register(dev, 65536, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC).unwrap();

    // Publish MR to node B's cache
    engine_b.mr_cache().insert(ub_core::mr::MrCacheEntry {
        owner_node: 1,
        remote_mr_handle: 1,
        base_ub_addr: mr_addr,
        len: 65536,
        perms: MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC,
        device_kind: DeviceKind::Memory,
    });

    // Allow connections to stabilize
    tokio::time::sleep(Duration::from_millis(100)).await;

    (engine_a, engine_b, mr_addr)
}

fn bench_write_1kib(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (engine_a, engine_b, mr_addr) = rt.block_on(setup_two_nodes());
    let _ = engine_a;

    let data = vec![0xABu8; 1024];

    let mut group = c.benchmark_group("write_1kib");
    group.throughput(Throughput::Bytes(1024));
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("write_1kib", |b| {
        b.iter(|| {
            rt.block_on(engine_b.ub_write_remote(mr_addr, &data))
        });
    });

    group.finish();
}

fn bench_read_1kib(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (engine_a, engine_b, mr_addr) = rt.block_on(setup_two_nodes());
    let _ = engine_a;

    // Write initial data first
    let data = vec![0xABu8; 1024];
    rt.block_on(engine_b.ub_write_remote(mr_addr, &data)).unwrap();

    let mut group = c.benchmark_group("read_1kib");
    group.throughput(Throughput::Bytes(1024));
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("read_1kib", |b| {
        b.iter(|| {
            rt.block_on(engine_b.ub_read_remote(mr_addr, 1024))
        });
    });

    group.finish();
}

fn bench_atomic_cas(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (engine_a, engine_b, mr_addr) = rt.block_on(setup_two_nodes());
    let _ = engine_a;

    // Initialize counter to 0
    let zero_data = 0u64.to_ne_bytes().to_vec();
    rt.block_on(engine_b.ub_write_remote(mr_addr, &zero_data)).unwrap();

    let mut group = c.benchmark_group("atomic_cas");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("atomic_cas", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            let old = counter;
            counter = counter.wrapping_add(1);
            rt.block_on(engine_b.ub_atomic_cas_remote(mr_addr, old, counter))
        });
    });

    group.finish();
}

fn bench_atomic_faa(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (engine_a, engine_b, mr_addr) = rt.block_on(setup_two_nodes());
    let _ = engine_a;

    // Initialize counter to 0
    let zero_data = 0u64.to_ne_bytes().to_vec();
    rt.block_on(engine_b.ub_write_remote(mr_addr, &zero_data)).unwrap();

    let mut group = c.benchmark_group("atomic_faa");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("atomic_faa", |b| {
        b.iter(|| {
            rt.block_on(engine_b.ub_atomic_faa_remote(mr_addr, 1))
        });
    });

    group.finish();
}

criterion_group!(benches, bench_write_1kib, bench_read_1kib, bench_atomic_cas, bench_atomic_faa);
criterion_main!(benches);
