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
