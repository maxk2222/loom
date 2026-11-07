use metrics::{counter, gauge, histogram};

pub fn req(status: &str) {
    counter!("loom_requests_total", "status" => status.to_string()).increment(1);
}
pub fn ttft(s: f64) {
    histogram!("loom_ttft_seconds").record(s);
}
pub fn queue_len(n: f64) {
    gauge!("loom_queue_depth").set(n);
}
pub fn client_online(v: bool) {
    gauge!("loom_clients_online").set(if v { 1.0 } else { 0.0 });
}
