use metrics::{counter, gauge, histogram};

pub fn req(status: &str) {
    counter!("loom_requests_total", "status" => status.to_string()).increment(1);
}
pub fn ttft(s: f64) {
    histogram!("loom_ttft_seconds").record(s);
}
pub fn queue_wait(s: f64) {
    histogram!("loom_queue_wait_seconds").record(s);
}
pub fn duration(s: f64) {
    histogram!("loom_request_duration_seconds").record(s);
}
pub fn queue_len(n: f64) {
    gauge!("loom_queue_depth").set(n);
}
pub fn clients_online(n: f64) {
    gauge!("loom_clients_online").set(n);
}
pub fn dep_kv(dep: &str, used: f64, budget: f64) {
    gauge!("loom_deployment_kv_used_tokens", "deployment" => dep.to_string()).set(used);
    gauge!("loom_deployment_kv_budget_tokens", "deployment" => dep.to_string()).set(budget);
}
pub fn lease_fp(n: f64) {
    histogram!("loom_lease_footprint_tokens").record(n);
}
/// Actual total tokens (prompt + completion) per finished request — the
/// heatmap source for "tokens per request over time".
pub fn req_tokens(n: f64) {
    histogram!("loom_request_tokens_total").record(n);
}
