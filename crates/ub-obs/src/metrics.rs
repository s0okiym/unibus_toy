//! Observability metrics — Prometheus-compatible counters, gauges, and histograms.
//!
//! FR-OBS-1: Each node exposes counters for tx/rx/retrans/drops/cqe/mr/jetty.
//! FR-OBS-3: Expose HTTP /metrics endpoint in Prometheus text format.

use metrics::{describe_counter, describe_gauge, describe_histogram};

// ── Metric name constants ──────────────────────────────────────────────

/// Total transmitted packets.
pub const TX_PKTS: &str = "unibus_tx_pkts_total";
/// Total received packets.
pub const RX_PKTS: &str = "unibus_rx_pkts_total";
/// Total retransmitted packets.
pub const RETRANS: &str = "unibus_retrans_total";
/// Total dropped packets.
pub const DROPS: &str = "unibus_drops_total";
/// Total successful CQE completions.
pub const CQE_OK: &str = "unibus_cqe_ok_total";
/// Total failed CQE completions.
pub const CQE_ERR: &str = "unibus_cqe_err_total";
/// Current MR count (gauge), labelled by device type.
pub const MR_COUNT: &str = "unibus_mr_count";
/// Current Jetty count (gauge).
pub const JETTY_COUNT: &str = "unibus_jetty_count";
/// Peer RTT in milliseconds (histogram, optional).
pub const PEER_RTT_MS: &str = "unibus_peer_rtt_ms";
/// Total placement decisions, labelled by tier (M7).
pub const PLACEMENT_DECISION: &str = "unibus_placement_decision_total";
/// Region cache hit count (M7).
pub const REGION_CACHE_HIT: &str = "unibus_region_cache_hit_total";
/// Region cache miss count (M7).
pub const REGION_CACHE_MISS: &str = "unibus_region_cache_miss_total";
/// Total invalidation messages sent (M7).
pub const INVALIDATE_SENT: &str = "unibus_invalidate_sent_total";
/// Writer lock wait time in ms (M7, histogram).
pub const WRITE_LOCK_WAIT_MS: &str = "unibus_write_lock_wait_ms";
/// Current region count, labelled by state (M7).
pub const REGION_COUNT: &str = "unibus_region_count";

// ── Describe all metrics ───────────────────────────────────────────────

/// Register metric descriptions with the metrics recorder.
/// Call this once at startup before any metric is used.
pub fn describe_metrics() {
    describe_counter!(TX_PKTS, "Total transmitted packets");
    describe_counter!(RX_PKTS, "Total received packets");
    describe_counter!(RETRANS, "Total retransmitted packets");
    describe_counter!(DROPS, "Total dropped packets");
    describe_counter!(CQE_OK, "Total successful CQE completions");
    describe_counter!(CQE_ERR, "Total failed CQE completions");
    describe_gauge!(MR_COUNT, "Current MR count");
    describe_gauge!(JETTY_COUNT, "Current Jetty count");
    describe_histogram!(PEER_RTT_MS, "Peer RTT in milliseconds");
    describe_counter!(PLACEMENT_DECISION, "Total placement decisions by tier");
    describe_counter!(REGION_CACHE_HIT, "Region cache hit count");
    describe_counter!(REGION_CACHE_MISS, "Region cache miss count");
    describe_counter!(INVALIDATE_SENT, "Total invalidation messages sent");
    describe_histogram!(WRITE_LOCK_WAIT_MS, "Writer lock wait time in ms");
    describe_gauge!(REGION_COUNT, "Current region count by state");
}

// ── Inline helper functions (for non-label cases) ──────────────────────

/// Increment a counter by 1 (no labels).
#[inline]
pub fn incr(name: &'static str) {
    metrics::counter!(name).increment(1);
}

/// Increment a counter by N (no labels).
#[inline]
pub fn incr_by(name: &'static str, delta: u64) {
    metrics::counter!(name).increment(delta);
}

/// Increment a counter by 1 with one static label.
#[inline]
pub fn incr_label(name: &'static str, label_key: &'static str, label_val: &'static str) {
    metrics::counter!(name, label_key => label_val).increment(1);
}

/// Set a gauge value (no labels).
#[inline]
pub fn set_gauge(name: &'static str, value: f64) {
    metrics::gauge!(name).set(value);
}

/// Increment a gauge by 1.0 (no labels).
#[inline]
pub fn gauge_up(name: &'static str) {
    metrics::gauge!(name).increment(1.0);
}

/// Decrement a gauge by 1.0 (no labels).
#[inline]
pub fn gauge_down(name: &'static str) {
    metrics::gauge!(name).decrement(1.0);
}

/// Set a gauge value with one static label.
#[inline]
pub fn set_gauge_label(name: &'static str, label_key: &'static str, label_val: &'static str, value: f64) {
    metrics::gauge!(name, label_key => label_val).set(value);
}

/// Record a histogram observation (no labels).
#[inline]
pub fn observe(name: &'static str, value: f64) {
    metrics::histogram!(name).record(value);
}

// ── Prometheus renderer ────────────────────────────────────────────────

use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::OnceLock;

/// Global Prometheus handle — installed once, reused across all callers.
static PROM_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder (once) and return the global handle.
/// Safe to call multiple times — subsequent calls return the existing handle.
pub fn install_recorder() -> &'static metrics_exporter_prometheus::PrometheusHandle {
    PROM_HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus metrics recorder")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_increment_counter() {
        let handle = install_recorder();
        describe_metrics();

        incr(TX_PKTS);
        incr(TX_PKTS);
        incr(TX_PKTS);

        let output = handle.render();
        assert!(output.contains("unibus_tx_pkts_total"), "output should contain tx_pkts metric");
    }

    #[test]
    fn test_metrics_gauge_with_labels() {
        let handle = install_recorder();
        describe_metrics();

        // Use metrics::gauge! directly with static labels
        metrics::gauge!(MR_COUNT, "device" => "memory").set(5.0);
        metrics::gauge!(MR_COUNT, "device" => "npu").set(2.0);

        let output = handle.render();
        assert!(output.contains("unibus_mr_count"), "output should contain mr_count metric");
        assert!(output.contains("device=\"memory\""), "output should contain memory label");
        assert!(output.contains("device=\"npu\""), "output should contain npu label");
    }

    #[test]
    fn test_prometheus_render_contains_prefix() {
        let handle = install_recorder();
        describe_metrics();

        incr(RX_PKTS);
        set_gauge(JETTY_COUNT, 3.0);

        let output = handle.render();
        for line in output.lines() {
            if line.starts_with("#") || line.is_empty() {
                continue;
            }
            assert!(
                line.starts_with("unibus_"),
                "metric line should start with unibus_ prefix: {line}"
            );
        }
    }
}