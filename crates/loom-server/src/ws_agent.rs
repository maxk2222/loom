//! WebSocket bridge for the agent channel.
//!
//! cloudflared's http2 tunnel protocol buffers request bodies until the
//! stream ends, so bidirectional gRPC can never finish a Hello through a
//! Cloudflare tunnel (and QUIC egress is not available everywhere).
//! WebSockets proxy live through any tunnel — these routes carry the same
//! envelopes on the HTTP gateway:
//!
//! - `GET /api/agent/ws/control` — binary WS frames = protobuf
//!   `ClientEnvelope`/`ServerEnvelope`, semantics identical to `Agent/Control`.
//! - `GET /api/agent/ws/data` — same bridge for `Agent/RequestData`
//!   (first frame must be the `Head`, fencing enforced unchanged).
//! - `POST /api/agent/config` — protobuf unary equivalent of `Agent/GetConfig`
//!   so WS-only clients never need a gRPC path.
//!
//! Auth model mirrors the gRPC listeners: no HTTP auth — the device
//! credential/enroll token inside Hello (or the lease fencing token in the
//! data-stream Head) gates access. `svc.listener` is `"ws"` for dashboards.

use crate::grpc_service::{config_snapshot, control_stream, request_data_stream, AgentSvc};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use loom_protocol::pb::*;
use prost::Message as _;
use tonic::Status;

/// gRPC streams cap at 1 MiB; WS frames get headroom for chunked payloads.
const MAX_WS_MSG: usize = 4 << 20;

pub fn router(svc: AgentSvc) -> Router {
    Router::new()
        .route("/api/agent/ws/control", get(control_ws))
        .route("/api/agent/ws/data", get(data_ws))
        .route("/api/agent/config", post(config_http))
        .with_state(svc)
}

async fn control_ws(State(svc): State<AgentSvc>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_WS_MSG)
        .on_upgrade(move |socket| control_conn(socket, svc))
}

#[allow(clippy::result_large_err)] // Status-shaped stream items, per protocol
async fn control_conn(socket: WebSocket, svc: AgentSvc) {
    let (mut tx, rx) = socket.split();
    let inbound = rx
        .take_while(|m| std::future::ready(!matches!(m, Ok(Message::Close(_)))))
        .map(|m| match m {
            Ok(Message::Binary(b)) => ClientEnvelope::decode(b.as_ref())
                .map_err(|e| Status::invalid_argument(format!("decode: {e}"))),
            Ok(_) => Err(Status::invalid_argument("binary frames only")),
            Err(e) => Err(Status::internal(format!("ws read: {e}"))),
        });
    let mut out = control_stream(svc, inbound);
    while let Some(item) = out.next().await {
        let Ok(env) = item else { break };
        if tx
            .send(Message::Binary(env.encode_to_vec().into()))
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = tx.close().await;
}

async fn data_ws(State(svc): State<AgentSvc>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_WS_MSG)
        .on_upgrade(move |socket| data_conn(socket, svc))
}

#[allow(clippy::result_large_err)] // Status-shaped stream items, per protocol
async fn data_conn(socket: WebSocket, svc: AgentSvc) {
    let (mut tx, rx) = socket.split();
    let inbound = rx
        .take_while(|m| std::future::ready(!matches!(m, Ok(Message::Close(_)))))
        .map(|m| match m {
            Ok(Message::Binary(b)) => DataFrame::decode(b.as_ref())
                .map_err(|e| Status::invalid_argument(format!("decode: {e}"))),
            Ok(_) => Err(Status::invalid_argument("binary frames only")),
            Err(e) => Err(Status::internal(format!("ws read: {e}"))),
        });
    // Pre-stream errors (missing head frame) surface as a close reason —
    // the gRPC equivalent is the call returning a Status before streaming.
    let mut out = match request_data_stream(svc, inbound).await {
        Ok(s) => s,
        Err(st) => {
            let _ = tx
                .send(Message::Close(Some(CloseFrame {
                    code: axum::extract::ws::close_code::ERROR,
                    reason: st.message().to_string().into(),
                })))
                .await;
            return;
        }
    };
    while let Some(item) = out.next().await {
        let Ok(frame) = item else { break };
        if tx
            .send(Message::Binary(frame.encode_to_vec().into()))
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = tx.close().await;
}

/// HTTP unary GetConfig for WS-only clients: protobuf in, protobuf out.
async fn config_http(State(svc): State<AgentSvc>, body: Bytes) -> Response {
    let req = match ConfigRequest::decode(body.as_ref()) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "invalid ConfigRequest protobuf".to_string(),
            )
                .into_response()
        }
    };
    match config_snapshot(&svc, req).await {
        Ok(snap) => (
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            snap.encode_to_vec(),
        )
            .into_response(),
        Err(st) => (status_to_http(&st), st.message().to_string()).into_response(),
    }
}

fn status_to_http(st: &Status) -> StatusCode {
    match st.code() {
        tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
