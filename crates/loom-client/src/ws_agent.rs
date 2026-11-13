//! WebSocket transport for the agent channel — mirrors the gRPC client
//! surface (`control`, `request_data`, `get_config`) over the HTTP
//! gateway routes. WS proxies request bodies live through tunnels that
//! buffer gRPC (cloudflared http2 without QUIC), making this the only
//! viable bidirectional path on Cloudflare-fronted public endpoints.
//!
//! Framing: one protobuf message per binary WS message, both directions.
//! Auth model is unchanged — enroll token/device credential in Hello,
//! lease fencing in the data Head frame; `wss` carries server-auth TLS
//! (system roots + optional pinned internal CA), `ws` is plaintext.

use anyhow::Context;
use futures_util::{SinkExt, Stream, StreamExt};
use loom_core::config::ClientConfig;
use loom_protocol::pb::*;
use prost::Message;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite;
use tonic::Status;

/// Decoded inbound stream matching tonic's `Streaming` item shape so the
/// control loop and request runner stay transport-agnostic.
pub type Inbound<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// `wss://`/`ws://` endpoint transport. Cheap to clone — every
/// `request_data` opens its own socket, mirroring gRPC per-RPC streams.
#[derive(Clone)]
pub struct WsAgent {
    /// `wss://host[/path]` base — `/api/agent/ws/*` paths are appended.
    base: String,
    /// WS-level TLS connector (None for plain ws://).
    connector: Option<tokio_tungstenite::Connector>,
    /// Shared reqwest client for the unary config route.
    http: reqwest::Client,
}

pub fn is_ws_url(url: &str) -> bool {
    url.starts_with("ws://") || url.starts_with("wss://")
}

/// `https://` → `wss://`, `http://` → `ws://`, ws schemes pass through.
pub fn ws_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    if let Some(r) = u.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = u.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        u.to_string()
    }
}

impl WsAgent {
    pub fn new(cfg: &ClientConfig, url: &str) -> anyhow::Result<Self> {
        let base = ws_url(url);
        let connector = if base.starts_with("wss://") {
            static PROVIDER: std::sync::Once = std::sync::Once::new();
            PROVIDER.call_once(|| {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            });
            let mut roots = rustls::RootCertStore::empty();
            // System roots — same trust base as tonic's with_enabled_roots.
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                roots.add(cert).ok();
            }
            // Pinned internal CA: wss endpoints behind our own TLS
            // (e.g. a tailnet-fronted gateway) verify against it too.
            if !cfg.connector.ca_cert_path.is_empty() {
                let pem = std::fs::read(&cfg.connector.ca_cert_path).with_context(|| {
                    format!(
                        "reading connector.ca_cert_path {}",
                        cfg.connector.ca_cert_path
                    )
                })?;
                let mut rd = std::io::BufReader::new(&pem[..]);
                for cert in rustls_pemfile::certs(&mut rd).flatten() {
                    roots.add(cert).ok();
                }
            }
            let tls = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(tls)))
        } else {
            None
        };
        Ok(Self {
            base,
            connector,
            http: reqwest::Client::new(),
        })
    }

    /// Open the control channel — returns the same halves gRPC yields:
    /// an outbound sender for `ClientEnvelope`s and an inbound stream of
    /// `ServerEnvelope`s (Status-errored on transport faults).
    pub async fn control(
        &self,
    ) -> anyhow::Result<(mpsc::Sender<ClientEnvelope>, Inbound<ServerEnvelope>)> {
        let (out_tx, out_rx) = mpsc::channel::<ClientEnvelope>(64);
        let inbound = self
            .pipe::<ClientEnvelope, ServerEnvelope>("/api/agent/ws/control", out_rx)
            .await?;
        Ok((out_tx, inbound))
    }

    /// Open a request-data socket: the caller feeds `DataFrame`s into
    /// `out` (Head first — fencing enforced server-side), receives the
    /// response frame stream back.
    pub async fn request_data(
        &self,
        out: mpsc::Receiver<DataFrame>,
    ) -> anyhow::Result<Inbound<DataFrame>> {
        self.pipe::<DataFrame, DataFrame>("/api/agent/ws/data", out)
            .await
    }

    /// Unary config pull over HTTP — protobuf in, protobuf out.
    pub async fn get_config(&self, req: ConfigRequest) -> anyhow::Result<ConfigSnapshot> {
        let url = format!("{}/api/agent/config", http_base(&self.base));
        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/x-protobuf")
            .body(prost::Message::encode_to_vec(&req))
            // Unary pull on the reconnect path: without a bound a stalled
            // socket wedges the control loop's restart (and heartbeats).
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("config request")?;
        let status = resp.status();
        let body = resp.bytes().await.context("config body")?;
        if !status.is_success() {
            anyhow::bail!(
                "config: HTTP {} {}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            );
        }
        ConfigSnapshot::decode(body.as_ref()).context("config decode")
    }

    /// Shared socket plumbing: writer task drains `out` into binary
    /// frames; the returned stream decodes inbound frames, ends on close.
    async fn pipe<Out, In>(
        &self,
        path: &str,
        out_rx: mpsc::Receiver<Out>,
    ) -> anyhow::Result<Inbound<In>>
    where
        Out: prost::Message + Send + 'static,
        In: prost::Message + Default + Send + 'static,
    {
        let url = format!("{}{}", self.base, path);
        let (socket, _resp) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_tls_with_config(
                url,
                None,
                false,
                self.connector.clone(),
            ),
        )
        .await
        .context("ws connect timeout")?
        .context("ws connect")?;
        let (mut ws_tx, ws_rx) = socket.split();
        let mut out_rx = out_rx;
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let bytes = msg.encode_to_vec();
                if ws_tx
                    .send(tungstenite::Message::Binary(bytes.into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = ws_tx.close().await;
        });
        let inbound = ws_rx.filter_map(|m| async move {
            match m {
                Ok(tungstenite::Message::Binary(b)) => Some(
                    In::decode(b.as_ref())
                        .map_err(|e| Status::invalid_argument(format!("ws decode: {e}"))),
                ),
                Ok(tungstenite::Message::Close(_)) => Some(Err(Status::unavailable("ws closed"))),
                Err(e) => Some(Err(Status::unavailable(format!("ws read: {e}")))),
                Ok(_) => None, // ping/pong/text — protocol noise
            }
        });
        Ok(Box::pin(inbound))
    }
}

/// `wss://`/`ws://` → `https://`/`http://` base for the unary routes.
fn http_base(url: &str) -> String {
    let u = url.trim_end_matches('/');
    if let Some(r) = u.strip_prefix("wss://") {
        format!("https://{r}")
    } else if let Some(r) = u.strip_prefix("ws://") {
        format!("http://{r}")
    } else {
        u.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_schemes() {
        assert_eq!(ws_url("https://grpc.example.com"), "wss://grpc.example.com");
        assert_eq!(ws_url("http://h:8080"), "ws://h:8080");
        assert_eq!(ws_url("wss://a"), "wss://a");
        assert_eq!(ws_url("https://a/"), "wss://a");
    }

    #[test]
    fn is_ws_url_detection() {
        assert!(is_ws_url("wss://a"));
        assert!(is_ws_url("ws://a:8080"));
        assert!(!is_ws_url("https://a"));
        assert!(!is_ws_url("http://a"));
    }

    #[test]
    fn http_base_maps_back() {
        assert_eq!(
            http_base("wss://grpc.example.com"),
            "https://grpc.example.com"
        );
        assert_eq!(http_base("ws://h:8080"), "http://h:8080");
    }

    /// End-to-end framing check against a minimal axum WS bridge wired
    /// exactly like the server's `ws_agent` module: binary frames carry
    /// one protobuf message each way. Exercises `control`, `request_data`
    /// and the unary `get_config` over a real socket.
    #[tokio::test]
    async fn ws_roundtrip_against_axum_bridge() {
        use axum::{
            extract::{
                ws::{Message as WsMsg, WebSocket, WebSocketUpgrade},
                State,
            },
            response::Response,
            routing::{get, post},
            Router,
        };
        use bytes::Bytes;

        async fn control_up(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(|mut sock: WebSocket| async move {
                while let Some(Ok(WsMsg::Binary(b))) = sock.recv().await {
                    let Ok(env) = ClientEnvelope::decode(b.as_ref()) else {
                        continue;
                    };
                    if matches!(env.msg, Some(client_envelope::Msg::Hello(_))) {
                        let ack = ServerEnvelope {
                            msg: Some(server_envelope::Msg::HelloAck(HelloAck {
                                accepted: true,
                                session_id: "s-test".into(),
                                client_id: "c-test".into(),
                                ..Default::default()
                            })),
                        };
                        let _ = sock.send(WsMsg::Binary(ack.encode_to_vec().into())).await;
                    }
                }
            })
        }
        async fn data_up(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(|mut sock: WebSocket| async move {
                while let Some(Ok(WsMsg::Binary(b))) = sock.recv().await {
                    if DataFrame::decode(b.as_ref()).is_ok() {
                        let pong = DataFrame {
                            frame: Some(data_frame::Frame::ResponseChunk(b"pong".to_vec())),
                        };
                        let _ = sock.send(WsMsg::Binary(pong.encode_to_vec().into())).await;
                        return;
                    }
                }
            })
        }
        async fn config_http(State(_): State<()>, body: Bytes) -> Vec<u8> {
            let _ = ConfigRequest::decode(body.as_ref());
            ConfigSnapshot {
                config_version: 7,
                ..Default::default()
            }
            .encode_to_vec()
        }

        let app = Router::new()
            .route("/api/agent/ws/control", get(control_up))
            .route("/api/agent/ws/data", get(data_up))
            .route("/api/agent/config", post(config_http))
            .with_state(());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let cfg = ClientConfig::default();
        let agent = WsAgent::new(&cfg, &format!("ws://{addr}")).unwrap();

        // Control: Hello out, HelloAck back.
        let (tx, mut inbound) = agent.control().await.unwrap();
        tx.send(ClientEnvelope {
            msg: Some(client_envelope::Msg::Hello(Hello::default())),
        })
        .await
        .unwrap();
        let env = tokio::time::timeout(Duration::from_secs(5), inbound.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match env.msg {
            Some(server_envelope::Msg::HelloAck(ack)) => {
                assert!(ack.accepted);
                assert_eq!(ack.client_id, "c-test");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Data: Head out, a DataFrame back.
        let (dtx, drx) = mpsc::channel(4);
        let mut din = agent.request_data(drx).await.unwrap();
        dtx.send(DataFrame {
            frame: Some(data_frame::Frame::Head(FrameHead {
                request_id: "r1".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let df = tokio::time::timeout(Duration::from_secs(5), din.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(df.frame, Some(data_frame::Frame::ResponseChunk(b)) if b == b"pong"));

        // Config unary.
        let snap = agent
            .get_config(ConfigRequest {
                client_id: "c-test".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(snap.config_version, 7);
    }
}
