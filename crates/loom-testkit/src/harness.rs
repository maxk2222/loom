//! Test harness helpers: proto-compat check, free ports, wait utilities.

use std::time::Duration;

/// Check proto range compatibility (mirror of loom_protocol::version::compatible,
/// re-exposed so testkit works standalone).
pub fn proto_compatible(peer_min: u32, peer_max: u32) -> bool {
    loom_protocol::version::compatible(peer_min, peer_max)
}

/// Poll `f` until true or timeout elapses. Returns final value.
pub async fn wait_until<F, Fut>(mut f: F, timeout: Duration, poll: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if f().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Redis URL for tests (env-gated).
pub fn test_redis_url() -> Option<String> {
    std::env::var("LOOM_TEST_REDIS_URL").ok()
}

/// Postgres URL for tests (env-gated).
pub fn test_database_url() -> Option<String> {
    std::env::var("LOOM_TEST_DATABASE_URL").ok()
}

/// Skip helper: returns true if infra env vars are present.
pub fn infra_available() -> bool {
    test_redis_url().is_some() && test_database_url().is_some()
}
