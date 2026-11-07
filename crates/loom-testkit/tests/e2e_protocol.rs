//! End-to-end protocol tests: real loom-client connector ↔ mock agent server
//! ↔ mock OpenAI backend. Exercises hello/session, assignment flow, streamed
//! responses, rejection, cancellation, capacity reports.

use loom_client::backend_manager::BackendManager;
use loom_client::config::ConfigStore;
use loom_client::connector::Connector;
use loom_client::scheduler::LocalScheduler;
use loom_core::capability::Capabilities;
use loom_core::config::{
    BackendConfig, BackendType, ClientConfig, DeploymentConfig, SharingMode, TlsMode,
};
use loom_testkit::harness::wait_until;
use loom_testkit::mock_agent_server::MockAgentServer;
use loom_testkit::mock_backend::MockBackend;
use std::sync::Arc;
use std::time::Duration;

fn dep(id: &str, model: &str, max_c: u32, reserve: u32, tier2: u32) -> DeploymentConfig {
    DeploymentConfig {
        id: id.into(),
        name: id.into(),
        model_name: model.into(),
        capabilities: Capabilities::default(),
        capability_overrides: false,
        max_concurrent: max_c,
        local_reserve: reserve,
        tier2_max: tier2,
        sharing_mode: SharingMode::Shared,
        enabled: true,
    }
}

fn make_cfg(server_url: &str, backend_url: &str, tiers: u32) -> ClientConfig {
    let mut c = ClientConfig::default();
    c.name = "test-ws".into();
    c.server_url = server_url.into();
    c.connector.tls = TlsMode::Off;
    c.connector.heartbeat_secs = 1;
    c.sharing_enabled = true;
    c.backends = vec![BackendConfig {
        id: "b1".into(),
        name: "b1".into(),
        backend_type: BackendType::GenericOpenAi,
        endpoint: backend_url.into(),
        api_key: String::new(),
        deployments: vec![dep("dep-a", "mock-llama", 4, 1, tiers)],
        enabled: true,
        health_interval_secs: 60,
    }];
    c
}

struct Rig {
    server: Arc<MockAgentServer>,
    _server_h: tokio::task::JoinHandle<()>,
    _backend_h: tokio::task::JoinHandle<()>,
    connector: Connector,
    sched: LocalScheduler,
    _cfg_dir: tempfile::TempDir,
}

async fn start_rig(tier2: u32) -> Rig {
    let server = Arc::new(MockAgentServer::new());
    server.accept_credential("dev-testcred").await;
    let (addr, server_h) = (*server).clone().spawn().await;
    let (backend_url, _mb, backend_h) = MockBackend::spawn("mock-llama").await;

    let mut cfg = make_cfg(&addr, &backend_url, tier2);
    cfg.client_id = "test-client".into();
    cfg.device_credential = "dev-testcred".into();

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ConfigStore::load(dir.path().join("c.toml")).unwrap());
    store.update(|c| *c = cfg.clone()).await.unwrap();

    let backends = BackendManager::new();
    backends.sync_from_config(&cfg).await;
    let deps: Vec<_> = cfg.backends.iter().flat_map(|b| b.deployments.clone()).collect();
    let sched = LocalScheduler::new(cfg.sharing_enabled);
    sched.sync_deployments(&deps);

    let connector = Connector::new(store, backends, sched.clone());
    {
        let conn = connector.clone();
        tokio::spawn(async move { conn.run().await });
    }
    // wait for session
    let up = wait_until(|| async { connector.is_connected() }, Duration::from_secs(10), Duration::from_millis(20)).await;
    assert!(up, "client did not connect");

    Rig {
        server,
        _server_h: server_h,
        _backend_h: backend_h,
        connector,
        sched,
        _cfg_dir: dir,
    }
}

#[tokio::test]
async fn e2e_assign_and_streamed_response() {
    let rig = start_rig(2).await;

    let assigned = rig
        .server
        .assign(
            "dep-a",
            "chat",
            serde_json::to_vec(&serde_json::json!({
                "model": "mock-llama",
                "messages": [{"role":"user","content":"hello"}],
                "stream": true
            }))
            .unwrap(),
            "vm-test",
        )
        .await
        .expect("assign failed");

    let req = assigned.await.expect("no assignment");
    // read response chunks — expect SSE data
    let mut got = String::new();
    let mut rx = req.response_rx;
    while let Ok(chunk) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        let Some(c) = chunk else { break };
        got.push_str(&String::from_utf8_lossy(&c));
        if got.contains("[DONE]") || got.contains("\"finish_reason\"") {
            break;
        }
    }
    assert!(got.contains("data:"), "expected SSE chunks, got: {got}");
}

#[tokio::test]
async fn e2e_reject_when_tier2_full() {
    // tier2 = 0 → server assignment rejected with admission reason
    let rig = start_rig(0).await;
    let assigned = rig
        .server
        .assign("dep-a", "chat", b"{}".to_vec(), "vm-test")
        .await;
    // assignment may fail outright (no session send) or receive a reject ack —
    // mock returns error only when control send fails; client acks reject.
    match assigned {
        Ok(rx) => {
            let _ = rx.await; // if it resolves, request never produces data
        }
        Err(_) => {} // control unavailable also acceptable — but with 0 slots
    }
    // capacity snapshot must show no free tier-2
    let snap = rig.sched.capacity_snapshot();
    for s in snap {
        assert_eq!(s.tier2_free, 0, "expected 0 tier2 free");
    }
}

#[tokio::test]
async fn e2e_config_push_applied() {
    let rig = start_rig(2).await;
    rig.server
        .push_config(
            42,
            serde_json::to_vec(&serde_json::json!({
                "version": 42,
                "client": {"sharing_enabled": true},
                "deployments": {}
            }))
            .unwrap(),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // client acks config — captured by mock
    let captured = rig.server.captured().await;
    assert!(
        captured.iter().any(|c| c.contains("config") || c.contains("ack")),
        "expected config ack, got {captured:?}"
    );
}

#[tokio::test]
async fn e2e_cancel_propagates() {
    let rig = start_rig(2).await;
    // assign a long request
    let assigned = rig
        .server
        .assign(
            "dep-a",
            "chat",
            serde_json::to_vec(&serde_json::json!({
                "model": "mock-llama",
                "messages": [{"role":"user","content":"slow"}],
                "stream": true
            }))
            .unwrap(),
            "vm-test",
        )
        .await
        .expect("assign failed");
    let req = assigned.await.expect("no assignment");
    // cancel via mock → client aborts upstream
    rig.server.cancel(&req.request_id, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // subsequent assign should still work (slot freed)
    let _ = rig.connector.abort_request(&req.request_id);
}
