//! Client registry: identity (Postgres) + live state (Redis) + sessions.

use crate::redisx::Rt;
use anyhow::Result;
use loom_core::ids::{sha256_hex, ClientId};
use loom_protocol::pb::{DeploymentCapacity, DeploymentReport, InventoryReport};
use serde_json::json;
use sqlx::PgPool;

#[derive(Clone)]
pub struct ClientRegistry {
    pub db: PgPool,
    pub rt: Rt,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ClientRow {
    pub id: uuid::Uuid,
    pub name: String,
    pub status: String,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub version: String,
    pub platform: String,
    pub labels: serde_json::Value,
    pub desired_config: serde_json::Value,
    pub config_version: i64,
}

impl ClientRegistry {
    /// Validate a credential: either an enroll token (first connect, creates
    /// the client row + issues device credential) or a device credential.
    /// Returns (client_id, issued_device_credential_or_empty).
    pub async fn authenticate(
        &self,
        client_id: &str,
        credential: &str,
        name: &str,
        version: &str,
        platform: &str,
    ) -> Result<Option<(String, String)>> {
        let hash = sha256_hex(credential);
        if !client_id.is_empty() {
            // Existing client: match id + credential hash, must be active.
            let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
                "SELECT id, status FROM clients WHERE id = $1::uuid AND credential_hash = $2",
            )
            .bind(client_id)
            .bind(&hash)
            .fetch_optional(&self.db)
            .await?;
            if let Some((id, status)) = row {
                if status != "active" {
                    return Ok(None);
                }
                sqlx::query(
                    "UPDATE clients SET last_seen_at = now(), version = $2, platform = $3 WHERE id = $1",
                )
                .bind(id)
                .bind(version)
                .bind(platform)
                .execute(&self.db)
                .await?;
                return Ok(Some((id.to_string(), String::new())));
            }
            return Ok(None);
        }

        // Enrollment: token must exist, be unexpired, have uses left.
        let tok: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT id FROM enroll_tokens WHERE token_hash = $1 AND used < max_uses AND expires_at > now()",
        )
        .bind(&hash)
        .fetch_optional(&self.db)
        .await?;
        let Some((tok_id,)) = tok else {
            return Ok(None);
        };

        let device_cred = format!("dev_{}", loom_core::ids::opaque_token(32));
        let cred_hash = sha256_hex(&device_cred);
        let cid = ClientId::new().0;
        sqlx::query(
            "INSERT INTO clients (id, name, credential_hash, status, version, platform) VALUES ($1,$2,$3,'active',$4,$5)
             ON CONFLICT (name) DO UPDATE SET credential_hash=$3, status='active', version=$4, platform=$5, last_seen_at=now()",
        )
        .bind(cid)
        .bind(if name.is_empty() { "client" } else { name })
        .bind(&cred_hash)
        .bind(version)
        .bind(platform)
        .execute(&self.db)
        .await?;
        sqlx::query("UPDATE enroll_tokens SET used = used + 1 WHERE id = $1")
            .bind(tok_id)
            .execute(&self.db)
            .await?;
        Ok(Some((cid.to_string(), device_cred)))
    }

    /// Mark a client + its deployments online; store session id.
    pub async fn set_online(&self, client_id: &str, session_id: &str, version: &str) -> Result<()> {
        let mut c = self.rt.conn.clone();
        let _: () = redis::pipe()
            .cmd("HSET")
            .arg(self.rt.k(&format!("client:{client_id}")))
            .arg("online")
            .arg("1")
            .arg("session_id")
            .arg(session_id)
            .arg("last_hb")
            .arg(now_ms())
            .arg("version")
            .arg(version)
            .ignore()
            .cmd("SET")
            .arg(self.rt.k(&format!("client:{client_id}:sess")))
            .arg(session_id)
            .ignore()
            .query_async(&mut c)
            .await?;
        sqlx::query("UPDATE clients SET last_seen_at = now() WHERE id = $1::uuid")
            .bind(client_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Heartbeat bookkeeping.
    pub async fn heartbeat(&self, client_id: &str, session_id: &str) -> Result<()> {
        let mut c = self.rt.conn.clone();
        let _: bool = redis::cmd("HSET")
            .arg(self.rt.k(&format!("client:{client_id}")))
            .arg("last_hb")
            .arg(now_ms())
            .query_async(&mut c)
            .await
            .unwrap_or(false);
        let _ = session_id;
        Ok(())
    }

    /// Process an inventory report: upsert deployments (Postgres reported side),
    /// update runtime dep hashes (Redis).
    pub async fn apply_inventory(&self, client_id: &str, inv: &InventoryReport) -> Result<()> {
        let mut c = self.rt.conn.clone();
        for d in &inv.deployments {
            upsert_deployment(&self.db, client_id, d).await?;
            write_dep_runtime(&mut c, &self.rt, client_id, d, None).await?;
        }
        self.apply_capacity(client_id, &inv.capacities).await?;
        Ok(())
    }

    /// Update runtime capacity hashes.
    pub async fn apply_capacity(
        &self,
        _client_id: &str,
        caps: &[DeploymentCapacity],
    ) -> Result<()> {
        let mut c = self.rt.conn.clone();
        for cap in caps {
            let key = self.rt.k(&format!("dep:{}", cap.deployment_id));
            let state = if cap.accepting {
                "available"
            } else {
                "draining"
            };
            let _: () = redis::cmd("HSET")
                .arg(&key)
                .arg("tier2_total")
                .arg(cap.tier2_total)
                .arg("tier2_free")
                .arg(cap.tier2_free)
                .arg("inflight_local")
                .arg(cap.inflight_local)
                .arg("inflight_tier2")
                .arg(cap.inflight_tier2)
                .arg("sharing_mode")
                .arg(&cap.sharing_mode)
                .arg("state")
                .arg(state)
                .arg("last_report")
                .arg(now_ms())
                .query_async(&mut c)
                .await?;
        }
        // wake the scheduler
        self.rt.publish("ev:capacity", "1").await.ok();
        Ok(())
    }

    /// Client went offline: run CLIENT_OFFLINE lua across its dep keys,
    /// return (requeue_request_ids, fail_request_ids).
    pub async fn client_offline(&self, client_id: &str) -> Result<(Vec<String>, Vec<String>)> {
        let mut c = self.rt.conn.clone();
        // find dep ids for this client
        let dep_ids: Vec<String> =
            sqlx::query_scalar("SELECT id::text FROM deployments WHERE client_id = $1::uuid")
                .bind(client_id)
                .fetch_all(&self.db)
                .await
                .unwrap_or_default();
        let _: () = redis::cmd("HSET")
            .arg(self.rt.k(&format!("client:{client_id}")))
            .arg("online")
            .arg("0")
            .query_async(&mut c)
            .await?;
        if dep_ids.is_empty() {
            return Ok((vec![], vec![]));
        }
        let mut keys = Vec::new();
        for d in &dep_ids {
            keys.push(self.rt.k(&format!("dep:{d}")));
            keys.push(self.rt.k(&format!("dep:{d}:leases")));
        }
        let script = redis::Script::new(crate::lua::CLIENT_OFFLINE);
        let res: Vec<Vec<String>> = script
            .key(keys)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();
        let reserved = res.first().cloned().unwrap_or_default();
        let active = res.get(1).cloned().unwrap_or_default();
        Ok((reserved, active))
    }

    /// Fetch deployment runtime state.
    pub async fn dep_runtime(&self, dep_id: &str) -> Result<Option<DepRuntime>> {
        let mut c = self.rt.conn.clone();
        let v: Option<std::collections::HashMap<String, String>> = redis::cmd("HGETALL")
            .arg(self.rt.k(&format!("dep:{dep_id}")))
            .query_async(&mut c)
            .await?;
        let Some(m) = v else { return Ok(None) };
        if m.is_empty() {
            return Ok(None);
        }
        Ok(Some(DepRuntime {
            state: m.get("state").cloned().unwrap_or_default(),
            tier2_free: m
                .get("tier2_free")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            tier2_total: m
                .get("tier2_total")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            client_id: m.get("client_id").cloned().unwrap_or_default(),
        }))
    }

    /// All deployments whose runtime state is `available`.
    pub async fn available_deployments(&self) -> Result<Vec<String>> {
        // Deployments live in Postgres (identity); runtime availability in Redis.
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id::text FROM deployments WHERE enabled = true")
                .fetch_all(&self.db)
                .await?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(rt) = self.dep_runtime(&id).await? {
                if rt.state == "available" && rt.tier2_free > 0 {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Live client view for the dashboard (Postgres + Redis merge).
    pub async fn live_clients(&self) -> Result<Vec<serde_json::Value>> {
        let rows: Vec<ClientRow> = sqlx::query_as(
            "SELECT id, name, status, last_seen_at, version, platform, labels, desired_config, config_version FROM clients ORDER BY name",
        )
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let cid = r.id.to_string();
            let mut c = self.rt.conn.clone();
            let online: Option<String> = redis::cmd("HGET")
                .arg(self.rt.k(&format!("client:{cid}")))
                .arg("online")
                .query_async(&mut c)
                .await
                .unwrap_or(None);
            out.push(json!({
                "id": cid,
                "name": r.name,
                "status": r.status,
                "online": online.as_deref() == Some("1"),
                "last_seen_at": r.last_seen_at,
                "version": r.version,
                "platform": r.platform,
                "labels": r.labels,
                "config_version": r.config_version,
            }));
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct DepRuntime {
    pub state: String,
    pub tier2_free: u32,
    pub tier2_total: u32,
    pub client_id: String,
}

async fn upsert_deployment(db: &PgPool, client_id: &str, d: &DeploymentReport) -> Result<()> {
    let caps = d.capabilities.as_ref().map(|c| {
        json!({
            "chat": c.chat, "completions": c.completions, "embeddings": c.embeddings,
            "tools": c.tools, "vision": c.vision, "json_mode": c.json_mode,
            "streaming": c.streaming, "context_length": c.context_length,
            "modalities": c.modalities,
        })
    });
    sqlx::query(
        "INSERT INTO deployments (id, client_id, name, backend_type, endpoint_url, model_name, capabilities, enabled, reported)
         VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (id) DO UPDATE SET name=$3, backend_type=$4, endpoint_url=$5,
           model_name=$6, capabilities=$7, reported=$9, updated_at=now()",
    )
    .bind(&d.deployment_id)
    .bind(client_id)
    .bind(&d.name)
    .bind(&d.backend_type)
    .bind(&d.endpoint_url)
    .bind(&d.model_name)
    .bind(caps.unwrap_or_else(|| json!({})))
    .bind(d.enabled)
    .bind(json!({"health": d.health}))
    .execute(db)
    .await?;
    Ok(())
}

async fn write_dep_runtime(
    c: &mut redis::aio::ConnectionManager,
    rt: &Rt,
    client_id: &str,
    d: &DeploymentReport,
    state: Option<&str>,
) -> Result<()> {
    let key = rt.k(&format!("dep:{}", d.deployment_id));
    let mut pipe = redis::pipe();
    pipe.cmd("HSET")
        .arg(&key)
        .arg("client_id")
        .arg(client_id)
        .arg("model_name")
        .arg(&d.model_name)
        .arg("health")
        .arg(&d.health)
        .arg("last_report")
        .arg(now_ms());
    if let Some(s) = state {
        pipe.arg("state").arg(s);
    }
    // capabilities JSON for routing filter
    if let Some(cp) = &d.capabilities {
        pipe.arg("capabilities").arg(
            serde_json::to_string(&json!({
                "chat": cp.chat, "completions": cp.completions, "embeddings": cp.embeddings,
                "tools": cp.tools, "vision": cp.vision, "json_mode": cp.json_mode,
                "streaming": cp.streaming, "context_length": cp.context_length,
            }))
            .unwrap_or_default(),
        );
    }
    let _: () = pipe.ignore().query_async(c).await?;
    Ok(())
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
