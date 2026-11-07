//! Local priority API integration tests: real axum listener + mock backend.

use loom_client::backend_manager::BackendManager;
use loom_client::config::ConfigStore;
use loom_client::connector::Connector;
use loom_client::local_api::{self, AppState};
use loom_client::scheduler::LocalScheduler;
use loom_core::config::{BackendConfig, BackendType, ClientConfig, DeploymentConfig, SharingMode};
use loom_testkit::harness::{test_redis_url, wait_until};
use loom_testkit::mock_backend::{MockBackend, MockBehavior};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn dep(id: &str, model: &str, max_concurrent: u32, tier2_max: u32) -> DeploymentConfig {
    DeploymentConfig {
        id: id.into(),
        name: id.into(),
        model_name: model.into(),
        capabilities: Default::default(),
        capability_overrides: false,
        max_concurrent,
        local_reserve: 0,
        tier2_max,
        auto_load: false,
        sharing_mode: SharingMode::Shared,
        enabled: true,
    }
}

fn backend(id: &str, url: &str, deps: Vec<DeploymentConfig>) -> BackendConfig {
    BackendConfig {
        id: id.into(),
        name: id.into(),
        backend_type: BackendType::GenericOpenAi,
        endpoint: url.into(),
        api_key: String::new(),
        deployments: deps,
        enabled: true,
        health_interval_secs: 60,
    }
}

async fn spawn_client_api(cfg: ClientConfig) -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg;
    c.connector.tls = loom_core::config::TlsMode::Off;
    let store = Arc::new(ConfigStore::load(dir.path().join("config.toml")).unwrap());
    store.update(|s| *s = c.clone()).await.unwrap();
    let backends = BackendManager::new();
    backends.sync_from_config(&c).await;
    let dep_cfgs: Vec<_> = c
        .backends
        .iter()
        .flat_map(|b| b.deployments.clone())
        .collect();
    let sched = LocalScheduler::new(true);
    sched.sync_deployments(&dep_cfgs);
    let connector = Connector::new(store.clone(), backends.clone(), sched.clone());
    let state = AppState {
        cfg: store,
        backends,
        scheduler: sched,
        connector,
        started: Instant::now(),
        requests: Arc::new(std::sync::RwLock::new(Default::default())),
    };
    let app = local_api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn local_api_models_and_chat_sse() {
    let (backend_url, _mb, _h) = MockBackend::spawn("mock-llama").await;
    let cfg = ClientConfig {
        name: "test-client".into(),
        backends: vec![backend(
            "b1",
            &backend_url,
            vec![dep("d1", "mock-llama", 4, 2)],
        )],
        ..Default::default()
    };
    let base = spawn_client_api(cfg).await;

    // models list contains our deployment
    let v: serde_json::Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<String> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.iter().any(|i| i.contains("mock-llama")),
        "models: {ids:?}"
    );

    // non-streaming chat
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "mock-llama", "messages": [{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "chat status {}", r.status());
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");

    // streaming chat returns SSE
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "mock-llama", "messages": [{"role":"user","content":"hi"}], "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let ct = r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("text/event-stream"), "ct={ct}");
    let text = r.text().await.unwrap();
    assert!(text.contains("data:"), "no sse data: {text}");
}

#[tokio::test]
async fn local_api_429_under_load_and_cancel() {
    let (backend_url, mb, _h) = MockBackend::spawn("mock-llama").await;
    mb.set_behavior(MockBehavior {
        ttft: Duration::from_millis(300),
        ..Default::default()
    });
    let cfg = ClientConfig {
        backends: vec![backend(
            "b1",
            &backend_url,
            vec![dep("d1", "mock-llama", 1, 0)],
        )],
        ..Default::default()
    };
    let base = spawn_client_api(cfg).await;

    // first request holds the only slot (don't await — let it run)
    let client = reqwest::Client::new();
    let inflight = {
        let c = client.clone();
        let base = base.clone();
        tokio::spawn(async move {
            c.post(format!("{base}/v1/chat/completions"))
                .json(&serde_json::json!({"model":"mock-llama","messages":[{"role":"user","content":"x"}]}))
                .send()
                .await
                .unwrap()
        })
    };
    // wait until backend actually processing
    let held = wait_until(
        || async { mb.in_flight() > 0 },
        Duration::from_secs(5),
        Duration::from_millis(10),
    )
    .await;
    assert!(held, "request never reached backend");

    // second request must be rejected 429 (max_concurrent=1, local class has priority but still bounded)
    let r = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model":"mock-llama","messages":[{"role":"user","content":"y"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 429, "expected 429, got {}", r.status());
    let e: serde_json::Value = r.json().await.unwrap();
    assert!(e["error"]["message"].is_string());

    inflight.abort();
    let _ = inflight.await;
}

#[tokio::test]
async fn local_api_requires_bearer_when_configured() {
    let (backend_url, _mb, _h) = MockBackend::spawn("mock-llama").await;
    let mut cfg = ClientConfig {
        backends: vec![backend(
            "b1",
            &backend_url,
            vec![dep("d1", "mock-llama", 4, 0)],
        )],
        ..Default::default()
    };
    cfg.local_api.api_key = "local-secret".into();
    let base = spawn_client_api(cfg).await;

    // no auth → 401
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model":"mock-llama","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);

    // correct key → ok
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("local-secret")
        .json(
            &serde_json::json!({"model":"mock-llama","messages":[{"role":"user","content":"hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
}

#[test]
fn redis_url_helper() {
    // sanity: harness reads env; not fatal when unset
    let _ = test_redis_url();
}
