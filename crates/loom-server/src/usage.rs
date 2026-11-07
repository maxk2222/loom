//! Usage pipeline: request-end events → requests_history + usage_counters
//! (batched writes), read APIs for the dashboard.

use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct UsageRecord {
    pub request_id: String,
    pub api_key_id: String,
    pub project_id: String,
    pub virtual_model_id: String,
    pub deployment_id: String,
    pub client_id: String,
    pub status: String,
    pub queue_wait_ms: i64,
    pub ttft_ms: i64,
    pub duration_ms: i64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub error_code: String,
    pub routing_trace: Value,
}

#[derive(Clone)]
pub struct UsageWriter {
    tx: mpsc::Sender<UsageRecord>,
}

impl UsageWriter {
    pub fn new(db: PgPool, retention_days: u32) -> Self {
        let (tx, mut rx) = mpsc::channel::<UsageRecord>(2048);
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(64);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            let mut last_cleanup = std::time::Instant::now();
            loop {
                tokio::select! {
                    Some(r) = rx.recv() => {
                        buf.push(r);
                        if buf.len() >= 64 {
                            flush(&db, &mut buf).await;
                        }
                    }
                    _ = tick.tick() => {
                        if !buf.is_empty() {
                            flush(&db, &mut buf).await;
                        }
                        if last_cleanup.elapsed() > std::time::Duration::from_secs(3600) {
                            last_cleanup = std::time::Instant::now();
                            let _ = sqlx::query(
                                "DELETE FROM requests_history WHERE created_at < now() - ($1 || ' days')::interval",
                            )
                            .bind(retention_days.to_string())
                            .execute(&db)
                            .await;
                        }
                    }
                }
            }
        });
        Self { tx }
    }

    pub async fn record(&self, r: UsageRecord) {
        let _ = self.tx.send(r).await;
    }
}

async fn flush(db: &PgPool, buf: &mut Vec<UsageRecord>) {
    for r in buf.drain(..) {
        let _ = sqlx::query(
            "INSERT INTO requests_history
             (id, api_key_id, project_id, virtual_model_id, deployment_id, client_id,
              status, queue_wait_ms, ttft_ms, duration_ms, prompt_tokens,
              completion_tokens, error_code, routing_trace)
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5::uuid, $6::uuid,
                     $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(&r.request_id)
        .bind(opt_uuid(&r.api_key_id))
        .bind(opt_uuid(&r.project_id))
        .bind(opt_uuid(&r.virtual_model_id))
        .bind(opt_uuid(&r.deployment_id))
        .bind(opt_uuid(&r.client_id))
        .bind(&r.status)
        .bind(r.queue_wait_ms)
        .bind(r.ttft_ms)
        .bind(r.duration_ms)
        .bind(r.prompt_tokens as i64)
        .bind(r.completion_tokens as i64)
        .bind(&r.error_code)
        .bind(&r.routing_trace)
        .execute(db)
        .await;
        // hourly counters
        if !r.project_id.is_empty() && !r.api_key_id.is_empty() && !r.virtual_model_id.is_empty() {
            let _ = sqlx::query(
                "INSERT INTO usage_counters (bucket, project_id, api_key_id, virtual_model_id, requests, prompt_tokens, completion_tokens)
                 VALUES (date_trunc('hour', now()), $1::uuid, $2::uuid, $3::uuid, 1, $4, $5)
                 ON CONFLICT (bucket, project_id, api_key_id, virtual_model_id)
                 DO UPDATE SET requests = usage_counters.requests + 1,
                               prompt_tokens = usage_counters.prompt_tokens + $4,
                               completion_tokens = usage_counters.completion_tokens + $5",
            )
            .bind(&r.project_id)
            .bind(&r.api_key_id)
            .bind(&r.virtual_model_id)
            .bind(r.prompt_tokens as i64)
            .bind(r.completion_tokens as i64)
            .execute(db)
            .await;
        }
    }
}

fn opt_uuid(s: &str) -> Option<uuid::Uuid> {
    uuid::Uuid::parse_str(s).ok()
}
