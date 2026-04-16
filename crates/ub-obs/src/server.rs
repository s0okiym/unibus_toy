//! /metrics HTTP handler — Prometheus text format output.
//!
//! This module provides an axum handler that renders all registered metrics
//! in Prometheus exposition format. The handler is mounted alongside the
//! admin API on the same port.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use metrics_exporter_prometheus::PrometheusHandle;

/// Axum handler for `GET /metrics`.
///
/// Returns Prometheus text format output with all registered counters,
/// gauges, and histograms.
pub async fn metrics_handler(handle: axum::extract::State<PrometheusHandle>) -> impl IntoResponse {
    let output = handle.render();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::Router;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn make_handle() -> &'static PrometheusHandle {
        crate::metrics::install_recorder()
    }

    #[tokio::test]
    async fn test_metrics_handler_returns_prometheus_format() {
        let handle = make_handle();
        crate::metrics::describe_metrics();
        crate::metrics::incr(crate::metrics::TX_PKTS);

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(handle.clone());

        let response = app
            .oneshot(axum::extract::Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(content_type.contains("text/plain"));

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("unibus_tx_pkts_total"));
    }
}