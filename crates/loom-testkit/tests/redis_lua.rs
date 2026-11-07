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
    assert_eq!(v4, redis::Value::SimpleString("NO_CAPACITY".into()));

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
async fn lease_sweep_expires_reserved_only() {
    let Some((mut c, p)) = conn().await else {
        return;
    };
    let dep = format!("{p}dep:d1");
    let leases = format!("{p}dep:d1:leases");
    let fencing_key = format!("{p}cap:fencing");

    // lease with 1ms ttl (expires immediately as far as sweep score goes)
    let v: redis::Value = redis::Script::new(lua::LEASE_ACQUIRE)
        .key(&dep)
        .key(&leases)
        .key(format!("{p}lease:e1"))
        .key(&fencing_key)
        .arg("req-e1")
        .arg("client-1")
        .arg("d1")
        .arg(1i64)
        .arg(now_ms() - 10_000)
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
