#![allow(clippy::result_large_err, clippy::too_many_arguments)]

//! loom-protocol: generated protobuf types + envelope helpers, versioning,
//! fencing utilities, and message-size limits shared by server and client.

pub mod pb {
    tonic::include_proto!("loom.agent.v1");
}

pub mod limits {
    /// Max control-stream message (AssignRequest etc.).
    pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
    /// Max single DataFrame payload.
    pub const MAX_DATAFRAME_BYTES: usize = 1024 * 1024;
    /// Max OpenAI request payload forwarded to a client.
    pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
    /// Protocol version implemented by this build.
    pub const PROTOCOL_VERSION: u32 = 1;
    /// Oldest protocol version this build accepts.
    pub const PROTOCOL_VERSION_MIN: u32 = 1;
}

pub mod version {
    use super::limits::{PROTOCOL_VERSION, PROTOCOL_VERSION_MIN};

    /// Returns true if the [peer_min, peer_max] range overlaps ours.
    pub fn compatible(peer_min: u32, peer_max: u32) -> bool {
        peer_max >= PROTOCOL_VERSION_MIN && peer_min <= PROTOCOL_VERSION
    }
}

/// Convenience constructors for envelope variants (server side).
pub mod server_msgs {
    use super::pb::*;

    pub fn hello_ack(
        accepted: bool,
        reject_reason: &str,
        session_id: &str,
        client_id: &str,
        device_credential: &str,
        config_version: u64,
        heartbeat_interval_secs: u32,
    ) -> ServerEnvelope {
        ServerEnvelope {
            msg: Some(server_envelope::Msg::HelloAck(HelloAck {
                accepted,
                reject_reason: reject_reason.into(),
                session_id: session_id.into(),
                client_id: client_id.into(),
                device_credential: device_credential.into(),
                config_version,
                heartbeat_interval_secs,
            })),
        }
    }

    pub fn assign(
        request_id: &str,
        lease_id: &str,
        fencing_token: u64,
        deadline_unix_ms: i64,
        api_type: &str,
        required_caps: RequiredCaps,
        payload_bytes: u32,
        virtual_model: &str,
        deployment_id: &str,
    ) -> ServerEnvelope {
        ServerEnvelope {
            msg: Some(server_envelope::Msg::AssignRequest(AssignRequest {
                request_id: request_id.into(),
                lease_id: lease_id.into(),
                fencing_token,
                deadline_unix_ms,
                api_type: api_type.into(),
                required_caps: Some(required_caps),
                payload_bytes,
                virtual_model: virtual_model.into(),
                deployment_id: deployment_id.into(),
            })),
        }
    }

    pub fn cancel(request_id: &str, fencing_token: u64, reason: &str) -> ServerEnvelope {
        ServerEnvelope {
            msg: Some(server_envelope::Msg::CancelRequest(CancelRequest {
                request_id: request_id.into(),
                fencing_token,
                reason: reason.into(),
            })),
        }
    }

    pub fn config_push(version: u64, payload: Vec<u8>, full_replace: bool) -> ServerEnvelope {
        ServerEnvelope {
            msg: Some(server_envelope::Msg::ConfigPush(ConfigPush {
                config_version: version,
                desired_config_json: payload,
                full_replace,
            })),
        }
    }

    pub fn ping() -> ServerEnvelope {
        ServerEnvelope {
            msg: Some(server_envelope::Msg::Ping(Ping {
                sent_at_unix_ms: now_ms(),
            })),
        }
    }

    pub fn now_ms() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    }
}

/// Convenience constructors for envelope variants (client side).
pub mod client_msgs {
    use super::pb::*;
    use super::server_msgs::now_ms;

    pub fn heartbeat(session_id: &str, inflight_local: u32, inflight_tier2: u32) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::Heartbeat(Heartbeat {
                session_id: session_id.into(),
                sent_at_unix_ms: now_ms(),
                inflight_local,
                inflight_tier2,
                load_1m: 0.0,
            })),
        }
    }

    pub fn capacity_report(capacities: Vec<DeploymentCapacity>) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::CapacityReport(CapacityReport {
                capacities,
                reported_at_unix_ms: now_ms().max(0) as u64,
            })),
        }
    }

    pub fn inventory(report: InventoryReport) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::InventoryReport(report)),
        }
    }

    pub fn config_ack(version: u64, applied: bool, error: &str) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::ConfigAck(ConfigAck {
                config_version: version,
                applied,
                error: error.into(),
            })),
        }
    }

    pub fn assign_accept(request_id: &str, lease_id: &str, fencing: u64) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::AssignAck(AssignAck {
                request_id: request_id.into(),
                lease_id: lease_id.into(),
                fencing_token: fencing,
                result: Some(assign_ack::Result::Accepted(true)),
            })),
        }
    }

    pub fn assign_reject(
        request_id: &str,
        lease_id: &str,
        fencing: u64,
        reason: &str,
        retryable: bool,
    ) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::AssignAck(AssignAck {
                request_id: request_id.into(),
                lease_id: lease_id.into(),
                fencing_token: fencing,
                result: Some(assign_ack::Result::Reject(Reject {
                    reason: reason.into(),
                    retryable,
                })),
            })),
        }
    }

    pub fn request_end(
        request_id: &str,
        lease_id: &str,
        status: &str,
        error_code: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        ttft_ms: i64,
        duration_ms: i64,
    ) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::RequestEnd(RequestEnd {
                request_id: request_id.into(),
                lease_id: lease_id.into(),
                status: status.into(),
                error_code: error_code.into(),
                prompt_tokens,
                completion_tokens,
                ttft_ms,
                duration_ms,
            })),
        }
    }

    pub fn error_report(request_id: &str, code: &str, message: &str) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::ErrorReport(ErrorReport {
                request_id: request_id.into(),
                code: code.into(),
                message: message.into(),
            })),
        }
    }

    pub fn pong(ping_sent_at_unix_ms: i64) -> ClientEnvelope {
        ClientEnvelope {
            msg: Some(client_envelope::Msg::Pong(Pong {
                ping_sent_at_unix_ms,
            })),
        }
    }
}

/// DataFrame helpers.
pub mod frames {
    use super::pb::*;

    pub fn head(request_id: &str, lease_id: &str, fencing_token: u64) -> DataFrame {
        DataFrame {
            frame: Some(data_frame::Frame::Head(FrameHead {
                request_id: request_id.into(),
                lease_id: lease_id.into(),
                fencing_token,
                role: "client".into(),
            })),
        }
    }

    pub fn request_json(chunk: Vec<u8>) -> DataFrame {
        DataFrame {
            frame: Some(data_frame::Frame::RequestJson(chunk)),
        }
    }

    pub fn response_chunk(chunk: Vec<u8>) -> DataFrame {
        DataFrame {
            frame: Some(data_frame::Frame::ResponseChunk(chunk)),
        }
    }

    pub fn end(cancelled: bool, finish_reason: &str) -> DataFrame {
        DataFrame {
            frame: Some(data_frame::Frame::End(End {
                cancelled,
                finish_reason: finish_reason.into(),
            })),
        }
    }

    pub fn error(code: &str, message: &str, retryable: bool) -> DataFrame {
        DataFrame {
            frame: Some(data_frame::Frame::Error(FrameError {
                code: code.into(),
                message: message.into(),
                retryable,
            })),
        }
    }
}
