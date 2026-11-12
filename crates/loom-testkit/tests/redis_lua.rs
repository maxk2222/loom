//! Redis Lua scheduling primitive tests — gated on LOOM_TEST_REDIS_URL.
//! Covers: lease double-acquire, fencing mismatch, release idempotency,
//! expiry sweep, client-offline reserved-vs-active split, queue FIFO, DRR pops.

use loom_server::lua;
use loom_testkit::harness::test_redis_url;
use redis::aio::ConnectionManager;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn conn() -> Option<(ConnectionManager, String)> {
    let url = test_redis_url()?;
    let client = redis::Client::open(url).ok()?;
    let mut c = ConnectionManager::new(client).await.ok()?;
    let prefix = format!("loom:test:{}:", uuid::Uuid::new_v4());
    // seed a deployment hash
    let dep = format!("{prefix}dep:d1");
    let _: () = redis::pipe()
        .cmd("HSET")
        .arg(&dep)
        .arg("state")
        .arg("available")
        .arg("tier2_free")
        .arg(2)
        .arg("tier2_total")
        .arg(2)
        .ignore()
        .query_async(&mut c)
        .await
        .ok()?;
    Some((c, prefix))
}

fn fencing_from(v: redis::Value) -> u64 {
    match v {
        redis::Value::Array(a) => match a.first() {
            Some(redis::Value::Int(i)) => *i as u64,
            _ => 0,
        },
        _ => 0,
    }
}

#[tokio::test]
async fn lease_acquire_double_and_fencing() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let lkey = format!("{p}lease:l1");
    let fencing_key = format!("{p}cap:fencing");

    // acquire → fencing token 1
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(&lkey)
        .key(&fencing_key)
        .arg("req-1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f1 = fencing_from(v);
    assert!(f1 > 0);

    // double-acquire by same request → idempotent (same fencing, still 1 slot used)
    let v2: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(&lkey)
        .key(&fencing_key)
        .arg("req-1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(fencing_from(v2), f1);
    let free: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("tier2_free")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(free, 1);

    // acquire for a second request → slot 2
    let v3: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:l2"))
        .key(&fencing_key)
        .arg("req-2")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f2 = fencing_from(v3);
    assert!(f2 > f1);

    // third → NO_CAPACITY
    let v4: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:l3"))
        .key(&fencing_key)
        .arg("req-3")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(
        redis::from_redis_value::<String>(&v4).unwrap(),
        "NO_CAPACITY"
    );

    // wrong fencing release → STALE, slot kept
    let r: String = redis::Script::new(lua::LEASE_RELEASE)
        .key(&lkey)
        .key(&dep)
        .key(&leases)
        .arg(f1 + 99)
        .arg("req-1")
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "STALE");
    let free: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("tier2_free")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(free, 0);

    // correct release → OK, slot back
    let r: String = redis::Script::new(lua::LEASE_RELEASE)
        .key(&lkey)
        .key(&dep)
        .key(&leases)
        .arg(f1)
        .arg("req-1")
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "OK");
    let free: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("tier2_free")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(free, 1);

    // idempotent re-release → NOT_FOUND (already gone)
    let r: String = redis::Script::new(lua::LEASE_RELEASE)
        .key(&lkey)
        .key(&dep)
        .key(&leases)
        .arg(f1)
        .arg("req-1")
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "NOT_FOUND");
}

#[tokio::test]
async fn client_offline_splits_reserved_and_active() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // reserved lease (req-r1)
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:r1"))
        .key(&fencing_key)
        .arg("req-r1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f1 = fencing_from(v);

    // active lease (req-a1): acquire then activate
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:a1"))
        .key(&fencing_key)
        .arg("req-a1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f2 = fencing_from(v);
    let _: String = redis::Script::new(lua::LEASE_ACTIVATE)
        .key(format!("{p}lease:a1"))
        .arg(f2)
        .arg(now_ms())
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert!(f1 > 0 && f2 > 0);

    // client_offline → reserved requeued, active failed
    let res: Vec<Vec<String>> = redis::Script::new(lua::CLIENT_OFFLINE)
        .key(&dep)
        .key(&leases)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(
        res[0],
        vec!["req-r1".to_string()],
        "reserved should requeue"
    );
    assert_eq!(res[1], vec!["req-a1".to_string()], "active should fail");

    // dep marked offline, leases gone
    let state: String = redis::cmd("HGET")
        .arg(&dep)
        .arg("state")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(state, "offline");
    let n: i64 = redis::cmd("ZCARD")
        .arg(&leases)
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn active_sweep_kills_only_idle_leases() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");
    let deadline = 600_000i64; // 10min idle window

    // two active leases, both activated long past the deadline
    for (name, req) in [("s1", "req-stale"), ("l1", "req-live")] {
        let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
            .key(&dep)
            .key(&leases)
            .key(format!("{p}lease:{name}"))
            .key(&fencing_key)
            .arg(req)
            .arg("client-1")
            .arg("d1")
            .arg(60_000i64)
            .arg(now_ms())
            .arg(0)
            .invoke_async(&mut c)
            .await
            .unwrap();
        let f = fencing_from(v);
        let _: String = redis::Script::new(lua::LEASE_ACTIVATE)
            .key(format!("{p}lease:{name}"))
            .arg(f)
            .arg(now_ms() - 700_000) // activated 700s ago
            .invoke_async(&mut c)
            .await
            .unwrap();
    }

    // simulate the bridge's liveness refresh: data still flowing on req-live
    let _: () = redis::cmd("HSET")
        .arg(format!("{p}lease:l1"))
        .arg("activated_at")
        .arg(now_ms() - 100_000)
        .query_async(&mut c)
        .await
        .unwrap();

    let timed_out: Vec<String> = redis::Script::new(lua::ACTIVE_LEASE_SWEEP)
        .key(&leases)
        .key(&dep)
        .arg(now_ms())
        .arg(deadline)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(
        timed_out,
        vec!["req-stale".to_string()],
        "only the silent lease must be killed"
    );

    // live lease still exists, dep got exactly one slot back
    let exists: i64 = redis::cmd("EXISTS")
        .arg(format!("{p}lease:l1"))
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(exists, 1);
    let free: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("tier2_free")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(free, 1);
}

#[tokio::test]
async fn lease_sweep_expires_reserved_only() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // lease already past its lease-score expiry — created 2min ago with a
    // 60s ttl, so the sweep score is in the past while the hash's own
    // PEXPIRE (ttl*3) still keeps it readable.
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:e1"))
        .key(&fencing_key)
        .arg("req-e1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms() - 120_000)
        .arg(0)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert!(fencing_from(v) > 0);

    let swept: Vec<String> = redis::Script::new(lua::LEASE_SWEEP)
        .key(&dep)
        .key(&leases)
        .arg(now_ms())
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(swept, vec!["req-e1".to_string()]);
    let free: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("tier2_free")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(free, 2);
}

#[tokio::test]
async fn queue_fifo_and_pop_order() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let q0 = format!("{p}queue:0");
    let q1 = format!("{p}queue:1");
    let req = |id: &str| format!("{p}req:{id}");
    let count = format!("{p}queue:count");
    let wd = format!("{p}req:watchdog");

    // enqueue two in class 1 (lower prio class number = higher priority here:
    // class 0 popped first), FIFO within class
    for (id, ts) in [("b", 2000i64), ("a", 1000)] {
        let r: String = redis::Script::new(lua::QUEUE_ENQUEUE)
            .key(&q1)
            .key(req(id))
            .key(&count)
            .key(&wd)
            .arg(format!("req-{id}"))
            .arg(ts)
            .arg(10i64)
            .arg("proj")
            .arg("vm")
            .arg(1i64)
            .arg(9_999_999_999i64)
            .invoke_async(&mut c)
            .await
            .unwrap();
        assert_eq!(r, "OK");
    }
    // class 0 gets one item with later ts — still pops first (class priority)
    let r: String = redis::Script::new(lua::QUEUE_ENQUEUE)
        .key(&q0)
        .key(req("c"))
        .key(&count)
        .key(&wd)
        .arg("req-c")
        .arg(5000i64)
        .arg(10i64)
        .arg("proj")
        .arg("vm")
        .arg(0i64)
        .arg(9_999_999_999i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "OK");

    async fn pop(c: &mut ConnectionManager, q0: &str, q1: &str) -> Option<String> {
        redis::Script::new(lua::QUEUE_POP)
            .key(q0)
            .key(q1)
            .invoke_async::<Option<String>>(c)
            .await
            .unwrap()
    }
    // class 0 first (priority), then FIFO inside class 1 (a before b)
    assert_eq!(pop(&mut c, &q0, &q1).await.as_deref(), Some("req-c"));
    assert_eq!(pop(&mut c, &q0, &q1).await.as_deref(), Some("req-a"));
    assert_eq!(pop(&mut c, &q0, &q1).await.as_deref(), Some("req-b"));
    assert_eq!(pop(&mut c, &q0, &q1).await, None);
}

#[tokio::test]
async fn queue_any_nonempty_reports_higher_class_waiters() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let q0 = format!("{p}queue:0");
    let q1 = format!("{p}queue:1");
    let q2 = format!("{p}queue:2");

    async fn any(c: &mut ConnectionManager, keys: Vec<String>) -> i64 {
        redis::Script::new(lua::QUEUE_ANY_NONEMPTY)
            .key(keys)
            .invoke_async::<i64>(c)
            .await
            .unwrap()
    }

    // all empty → 0 on any subset
    assert_eq!(any(&mut c, vec![q0.clone()]).await, 0);
    assert_eq!(any(&mut c, vec![q0.clone(), q1.clone()]).await, 0);

    // an entry in class 1 is seen when scanning 0..=1 (a class-1 submit
    // defers) but NOT when scanning only class 0 — a class-0 submit may
    // fast-path past lower-class waiters, which is strict ordering.
    redis::cmd("ZADD")
        .arg(&q1)
        .arg(1000i64)
        .arg("req-x")
        .query_async::<i64>(&mut c)
        .await
        .unwrap();
    assert_eq!(any(&mut c, vec![q0.clone()]).await, 0);
    assert_eq!(any(&mut c, vec![q0.clone(), q1.clone()]).await, 1);
    assert_eq!(
        any(&mut c, vec![q0.clone(), q1.clone(), q2.clone()]).await,
        1
    );

    // once the class-1 entry leaves, the scan reports empty again
    redis::cmd("ZREM")
        .arg(&q1)
        .arg("req-x")
        .query_async::<i64>(&mut c)
        .await
        .unwrap();
    assert_eq!(any(&mut c, vec![q0, q1, q2]).await, 0);
}

#[tokio::test]
async fn kv_budget_gates_acquire_and_release_returns_footprint() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // acquire with fp/budget args
    #[allow(clippy::too_many_arguments)]
    async fn acquire(
        c: &mut ConnectionManager,
        dep: &str,
        leases: &str,
        fencing_key: &str,
        lease: &str,
        req: &str,
        fp: i64,
        budget: i64,
    ) -> redis::Value {
        redis::Script::new(lua::LEASE_ACQUIRE)
            .key(dep)
            .key(leases)
            .key(lease)
            .key(fencing_key)
            .arg(req)
            .arg("client-1")
            .arg("d1")
            .arg(60_000i64)
            .arg(now_ms())
            .arg(0)
            .arg(fp)
            .arg(budget)
            .invoke_async::<redis::Value>(c)
            .await
            .unwrap()
    }

    let f1 = fencing_from(
        acquire(
            &mut c,
            &dep,
            &leases,
            &fencing_key,
            &format!("{p}lease:k1"),
            "req-k1",
            60_000,
            100_000,
        )
        .await,
    );
    assert!(f1 > 0);
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 60_000);

    // 50_000 would push kv_used to 110_000 > 100_000 → KV_FULL (not NO_CAPACITY:
    // slots remain — the gate is the token budget)
    let v = acquire(
        &mut c,
        &dep,
        &leases,
        &fencing_key,
        &format!("{p}lease:k2"),
        "req-k2",
        50_000,
        100_000,
    )
    .await;
    assert_eq!(redis::from_redis_value::<String>(&v).unwrap(), "KV_FULL");
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 60_000, "a rejected acquire must not reserve");

    // exact-boundary footprint still fits
    let f2 = fencing_from(
        acquire(
            &mut c,
            &dep,
            &leases,
            &fencing_key,
            &format!("{p}lease:k3"),
            "req-k3",
            40_000,
            100_000,
        )
        .await,
    );
    assert!(f2 > 0);
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 100_000);

    // release returns the footprint exactly once (re-release → NOT_FOUND, no double decrement)
    let r: String = redis::Script::new(lua::LEASE_RELEASE)
        .key(format!("{p}lease:k1"))
        .key(&dep)
        .key(&leases)
        .arg(f1)
        .arg("req-k1")
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "OK");
    let r2: String = redis::Script::new(lua::LEASE_RELEASE)
        .key(format!("{p}lease:k1"))
        .key(&dep)
        .key(&leases)
        .arg(f1)
        .arg("req-k1")
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r2, "NOT_FOUND");
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 40_000);

    // the freed headroom admits again
    let f3 = fencing_from(
        acquire(
            &mut c,
            &dep,
            &leases,
            &fencing_key,
            &format!("{p}lease:k4"),
            "req-k4",
            60_000,
            100_000,
        )
        .await,
    );
    assert!(f3 > 0);
}

#[tokio::test]
async fn kv_budget_zero_is_legacy_slot_only() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // budget=0 → footprint is not gated at all (tracked but unbounded)
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:g1"))
        .key(&fencing_key)
        .arg("req-g1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .arg(9_999_999i64) // absurd footprint
        .arg(0i64) // no budget
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert!(fencing_from(v) > 0);
}

#[tokio::test]
async fn kv_fp_update_is_monotonic_and_fenced() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let lkey = format!("{p}lease:u1");
    let fencing_key = format!("{p}cap:fencing");

    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(&lkey)
        .key(&fencing_key)
        .arg("req-u1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .arg(50_000i64)
        .arg(100_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f = fencing_from(v);

    // wrong fencing → STALE
    let r: String = redis::Script::new(lua::LEASE_FP_UPDATE)
        .key(&lkey)
        .key(&dep)
        .arg(f + 7)
        .arg(80_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "STALE");

    // growth applies the delta to dep.kv_used
    let r: String = redis::Script::new(lua::LEASE_FP_UPDATE)
        .key(&lkey)
        .key(&dep)
        .arg(f)
        .arg(80_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "OK");
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 80_000);

    // a lower (stale/lagging) report must not shrink the reservation
    let r: String = redis::Script::new(lua::LEASE_FP_UPDATE)
        .key(&lkey)
        .key(&dep)
        .arg(f)
        .arg(30_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(r, "OK");
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 80_000);
}

#[tokio::test]
async fn kv_used_returned_by_sweep_and_client_offline() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // expired reserved lease carrying a footprint (score past, hash alive —
    // same pattern as lease_sweep_expires_reserved_only)
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:s1"))
        .key(&fencing_key)
        .arg("req-s1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms() - 120_000)
        .arg(0)
        .arg(70_000i64)
        .arg(100_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert!(fencing_from(v) > 0);

    let swept: Vec<String> = redis::Script::new(lua::LEASE_SWEEP)
        .key(&dep)
        .key(&leases)
        .arg(now_ms())
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(swept, vec!["req-s1".to_string()]);
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 0, "swept lease must return its footprint");

    // active lease reaped by CLIENT_OFFLINE returns its footprint too
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:o1"))
        .key(&fencing_key)
        .arg("req-o1")
        .arg("client-1")
        .arg("d1")
        .arg(60_000i64)
        .arg(now_ms())
        .arg(0)
        .arg(90_000i64)
        .arg(100_000i64)
        .invoke_async(&mut c)
        .await
        .unwrap();
    let f = fencing_from(v);
    let _: String = redis::Script::new(lua::LEASE_ACTIVATE)
        .key(format!("{p}lease:o1"))
        .arg(f)
        .arg(now_ms())
        .invoke_async(&mut c)
        .await
        .unwrap();
    let res: Vec<Vec<String>> = redis::Script::new(lua::CLIENT_OFFLINE)
        .key(&dep)
        .key(&leases)
        .invoke_async(&mut c)
        .await
        .unwrap();
    assert_eq!(res[1], vec!["req-o1".to_string()]);
    let used: i64 = redis::cmd("HGET")
        .arg(&dep)
        .arg("kv_used")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(used, 0);
}

#[tokio::test]
async fn requeue_preserves_attempts() {
    // Regression: QUEUE_ENQUEUE used to HSET attempts='0' on every enqueue,
    // so a structurally-undispatchable request requeued with its counter
    // reset forever — head-of-line blocking the whole class queue.
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let q0 = format!("{p}queue:0");
    let req = format!("{p}req:r1");
    let count = format!("{p}queue:count");
    let wd = format!("{p}req:watchdog");

    async fn enq(
        c: &mut ConnectionManager,
        q0: &str,
        req: &str,
        count: &str,
        wd: &str,
        ts: i64,
    ) -> String {
        redis::Script::new(lua::QUEUE_ENQUEUE)
            .key(q0)
            .key(req)
            .key(count)
            .key(wd)
            .arg("req-r1")
            .arg(ts)
            .arg(10i64)
            .arg("proj")
            .arg("vm")
            .arg(0i64)
            .arg(9_999_999_999i64)
            .invoke_async::<String>(c)
            .await
            .unwrap()
    }

    assert_eq!(enq(&mut c, &q0, &req, &count, &wd, 1000).await, "OK");
    // simulate two failed dispatch attempts (scheduler HINCRBYs before requeue)
    for _ in 0..2 {
        let _: i64 = redis::cmd("HINCRBY")
            .arg(&req)
            .arg("attempts")
            .arg(1)
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(enq(&mut c, &q0, &req, &count, &wd, 1000).await, "OK");
    }
    let attempts: i64 = redis::cmd("HGET")
        .arg(&req)
        .arg("attempts")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(attempts, 2, "requeue must preserve the attempts counter");
}
