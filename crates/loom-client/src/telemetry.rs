//! Metrics (Prometheus facade) for the client daemon.

use metrics::{counter, gauge, histogram};

pub fn req_admitted(class: &str) {
    counter!("loom_client_requests_admitted_total", "class" => class.to_string()).increment(1);
}

pub fn req_rejected(class: &str, reason: &str) {
    counter!(
        "loom_client_requests_rejected_total",
        "class" => class.to_string(),
        "reason" => reason.to_string()
    )
    .increment(1);
}

pub fn req_finished(status: &str) {
    counter!("loom_client_requests_finished_total", "status" => status.to_string()).increment(1);
}

pub fn req_duration(secs: f64, class: &str) {
    histogram!("loom_client_request_duration_seconds", "class" => class.to_string()).record(secs);
}

pub fn ttft(secs: f64) {
    histogram!("loom_client_ttft_seconds").record(secs);
}

pub fn slots(dep: &str, kind: &str, value: u32) {
    gauge!("loom_client_slots", "deployment" => dep.to_string(), "kind" => kind.to_string())
        .set(value as f64);
}

pub fn server_connected(v: bool) {
    gauge!("loom_client_server_connected").set(if v { 1.0 } else { 0.0 });
}

pub fn backend_health(dep: &str, healthy: bool) {
    gauge!("loom_client_backend_healthy", "deployment" => dep.to_string())
        .set(if healthy { 1.0 } else { 0.0 });
}

/// Install a Prometheus exporter bound to the given address (separate from the
/// main API listener so /metrics stays small; returns the join handle).
pub async fn spawn_metrics_listener(
    bind: std::net::SocketAddr,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    use metrics_exporter_prometheus::PrometheusBuilder;
    let handle = PrometheusBuilder::new()
        .with_http_listener(bind)
        .install_recorder()?;
    // exporter runs its own listener task
    Ok(tokio::spawn(async move {
        let _ = handle;
        futures::future::pending::<()>().await;
    }))
}
