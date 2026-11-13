//! Instance-owner lock: exactly one loom-server per Redis namespace owns
//! the runtime state and may wipe it at boot (`reset_runtime`). The lock
//! is a short-lived `SET PX NX` key, renewed while held.
//!
//! Boot-time behaviour (`acquire`):
//! - free key → owner: reset runtime, keep renewing.
//! - held by a *dead* predecessor (SIGKILL, crash — no graceful release):
//!   wait out its remaining TTL (<= `ttl_ms`) instead of starting as a
//!   stateless peer; the stale-slots/dead-lease window that a skipped
//!   reset produces disappears.
//! - held by a *live* peer (its PTTL rises when it renews): bail fast —
//!   multi-instance keeps working and never pays the wait window.
//! - key without expiry (`PTTL = -1`, foreign garbage): never steal —
//!   wait out `wait_ms`, then peer.
//!
//! Graceful release: the owner CAS-deletes the key on SIGTERM/SIGINT and
//! before the self-update `exit(0)` (`release_if_owner`), so restarts
//! and updates hand over instantly with a full reset.

use std::sync::OnceLock;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::redisx::Rt;

static OWNER: OnceLock<(String, String)> = OnceLock::new();
static RENEWER: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    Owner,
    Peer,
}

#[derive(Debug, Clone)]
pub struct LockOpts {
    /// Lock lease (`PX`). Renewed every `renew_tick_ms` while owned.
    pub ttl_ms: u64,
    /// Max wait for a foreign-held lock before yielding to `Peer`.
    /// `0` = single-shot legacy behaviour. Must stay below the
    /// container healthcheck-unhealthy threshold (~50s) so a dead-owner
    /// restart still becomes healthy on first bind.
    pub wait_ms: u64,
    /// Delay between acquire attempts / PTTL samples.
    pub poll_ms: u64,
    /// CAS `PEXPIRE` cadence while owner (must be < `ttl_ms`).
    pub renew_tick_ms: u64,
}

impl Default for LockOpts {
    fn default() -> Self {
        Self {
            ttl_ms: 30_000,
            wait_ms: std::env::var("LOOM_INSTANCE_LOCK_WAIT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(35_000),
            poll_ms: 1_500,
            renew_tick_ms: 10_000,
        }
    }
}

fn peer_warn() {
    warn!(
        "another loom-server owns this Redis namespace — starting without \
         runtime reset (multi-instance: shared queue, per-instance waiters)"
    );
}

async fn try_set_nx(rt: &Rt, key: &str, token: &str, ttl_ms: u64) -> bool {
    let mut c = rt.conn.clone();
    let got: Option<String> = redis::cmd("SET")
        .arg(key)
        .arg(token)
        .arg("PX")
        .arg(ttl_ms)
        .arg("NX")
        .query_async(&mut c)
        .await
        .unwrap_or(None);
    got.is_some()
}

async fn pttl(rt: &Rt, key: &str) -> Option<i64> {
    let mut c = rt.conn.clone();
    redis::cmd("PTTL").arg(key).query_async(&mut c).await.ok()
}

/// Try to become the namespace owner. On `Owner` the caller must run
/// `reset_runtime()`, then register `spawn_renewer` via `set_renewer`.
pub async fn acquire(rt: &Rt, key: &str, token: &str, opts: &LockOpts) -> Acquire {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(opts.wait_ms);
    let mut prev_pttl: Option<i64> = None;
    let mut announced = false;
    loop {
        if try_set_nx(rt, key, token, opts.ttl_ms).await {
            let _ = OWNER.set((key.to_string(), token.to_string()));
            return Acquire::Owner;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if opts.wait_ms == 0 || remaining.is_zero() {
            peer_warn();
            return Acquire::Peer;
        }
        if !announced {
            info!("instance lock held — waiting for release");
            announced = true;
        }
        // Live-owner detection: only the holder's CAS renew (or a fresh
        // acquirer) can push PTTL up — a dead owner's PTTL decreases to
        // expiry. A stuck no-expiry key (-1) never rises and is never
        // stolen; it just burns the wait window.
        if let Some(cur) = pttl(rt, key).await {
            if prev_pttl.is_some_and(|prev| cur > prev) {
                peer_warn();
                return Acquire::Peer;
            }
            prev_pttl = Some(cur);
        }
        tokio::time::sleep(Duration::from_millis(opts.poll_ms).min(remaining)).await;
    }
}

/// Renew the held lock until shutdown. If the CAS ever fails (our lease
/// expired during a Redis blip) re-acquire only when the key is FREE —
/// never steal, never reset mid-run. Ownership loss is logged once.
pub fn spawn_renewer(
    rt: Rt,
    key: String,
    token: String,
    ttl_ms: u64,
    tick_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
        let mut warned = false;
        loop {
            tick.tick().await;
            let mut c = rt.conn.clone();
            let renewed: i64 = redis::Script::new(
                "if redis.call('GET', KEYS[1]) == ARGV[1] \
                 then return redis.call('PEXPIRE', KEYS[1], ARGV[2]) \
                 else return 0 end",
            )
            .key(&key)
            .arg(&token)
            .arg(ttl_ms)
            .invoke_async(&mut c)
            .await
            .unwrap_or(0);
            if renewed == 0 {
                if try_set_nx(&rt, &key, &token, ttl_ms).await {
                    if warned {
                        info!("instance ownership re-acquired");
                        warned = false;
                    }
                } else if !warned {
                    warned = true;
                    warn!("instance ownership lost — will re-acquire if the lock frees");
                }
            } else {
                warned = false; // healthy renew again — next loss warns fresh
            }
        }
    })
}

/// CAS-delete the lock: only the recorded owner token may delete.
/// Bounded by a short timeout so a dead Redis cannot stall shutdown.
pub async fn release(rt: &Rt, key: &str, token: &str) {
    let mut c = rt.conn.clone();
    let res = tokio::time::timeout(Duration::from_secs(2), async {
        redis::Script::new(
            "if redis.call('GET', KEYS[1]) == ARGV[1] \
             then return redis.call('DEL', KEYS[1]) \
             else return 0 end",
        )
        .key(key)
        .arg(token)
        .invoke_async::<i64>(&mut c)
        .await
    })
    .await;
    match res {
        Ok(Ok(1)) => debug!("instance lock released"),
        Ok(Ok(_)) => debug!("instance lock already changed hands"),
        Ok(Err(e)) => warn!(error = %e, "instance lock release failed"),
        Err(_) => warn!("instance lock release timed out"),
    }
}

/// Register the running renewer so `release_if_owner` can stop it.
pub fn set_renewer(handle: tokio::task::JoinHandle<()>) {
    *RENEWER.lock().unwrap() = Some(handle);
}

/// Graceful release for the boot-time owner — no-op on a `Peer` boot.
/// Called on SIGTERM/SIGINT and before the self-update `exit(0)` so
/// restarts hand over instantly instead of expiring the TTL.
pub async fn release_if_owner(rt: &Rt) {
    // Stop the renewer first and wait for teardown: an in-flight tick
    // could otherwise re-acquire the just-released key microseconds
    // later, re-creating the exact dead-owner wait this avoids.
    let handle = RENEWER.lock().unwrap().take();
    if let Some(h) = handle {
        h.abort();
        let _ = h.await;
    }
    if let Some((key, token)) = OWNER.get() {
        release(rt, key, token).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_rt() -> Option<Rt> {
        let url = std::env::var("LOOM_TEST_REDIS_URL").ok()?;
        let prefix = format!("ilock:{}:", loom_core::ids::opaque_token(6));
        Rt::connect(&url, &prefix).await.ok()
    }

    fn opts(wait_ms: u64) -> LockOpts {
        LockOpts {
            ttl_ms: 300,
            wait_ms,
            poll_ms: 50,
            renew_tick_ms: 80,
        }
    }

    #[tokio::test]
    async fn acquire_waits_out_dead_owner() {
        let Some(rt) = test_rt().await else { return };
        let key = rt.k("instance:owner");
        assert!(try_set_nx(&rt, &key, "dead", 300).await);
        let t0 = std::time::Instant::now();
        assert_eq!(
            acquire(&rt, &key, "mine", &opts(3_000)).await,
            Acquire::Owner
        );
        assert!(t0.elapsed() >= Duration::from_millis(280));
    }

    #[tokio::test]
    async fn live_owner_bails_on_renew() {
        let Some(rt) = test_rt().await else { return };
        let key = rt.k("instance:owner");
        assert!(try_set_nx(&rt, &key, "live", 600).await);
        let h = spawn_renewer(rt.clone(), key.clone(), "live".into(), 600, 80);
        let t0 = std::time::Instant::now();
        assert_eq!(
            acquire(&rt, &key, "mine", &opts(5_000)).await,
            Acquire::Peer
        );
        assert!(t0.elapsed() < Duration::from_secs(2));
        h.abort();
    }

    #[tokio::test]
    async fn no_expiry_lock_is_never_stolen() {
        let Some(rt) = test_rt().await else { return };
        let key = rt.k("instance:owner");
        let mut c = rt.conn.clone();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("stuck")
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(acquire(&rt, &key, "mine", &opts(400)).await, Acquire::Peer);
    }

    #[tokio::test]
    async fn release_deletes_only_for_owner_token() {
        let Some(rt) = test_rt().await else { return };
        let key = rt.k("instance:owner");
        assert!(try_set_nx(&rt, &key, "tok1", 30_000).await);
        release(&rt, &key, "wrong").await;
        let mut c = rt.conn.clone();
        let v: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some("tok1"));
        release(&rt, &key, "tok1").await;
        let v: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut c)
            .await
            .unwrap();
        assert!(v.is_none());
    }
}
