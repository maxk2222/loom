//! Client registry: identity (Postgres) + live state (Redis) + sessions.

use crate::redisx::Rt;
use anyhow::Result;
use loom_core::ids::{sha256_hex, ClientId};
use loom_protocol::pb::{DeploymentCapacity, DeploymentReport, InventoryReport};
use serde_json::{json, Value};
use sqlx::PgPool;

#[derive(Clone)]
pub struct ClientRegistry {
    pub db: PgPool,
    pub rt: Rt,
    /// Model routing snapshot — patched live so inventory reports reach the
    /// scheduler's capability filter without waiting for an admin reload.
    pub models: crate::model_registry::ModelRegistry,
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
        if credential.is_empty() {
            return Ok(None);
        }
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
                // Name renames propagate on reconnect — skip silently when
                // the desired name is already taken by another client.
                sqlx::query(
                    "UPDATE clients SET last_seen_at = now(), version = $2, platform = $3,
                     name = CASE WHEN $4 <> '' AND NOT EXISTS (SELECT 1 FROM clients WHERE name = $4 AND id <> $1) THEN $4 ELSE name END
                     WHERE id = $1",
                )
                .bind(id)
                .bind(version)
                .bind(platform)
                .bind(name)
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
        // RETURNING id: on a name conflict the existing row keeps its id — the
        // client must learn that id, not the freshly generated one, or every
        // subsequent credential check misses the row.
        let row_id: (uuid::Uuid,) = sqlx::query_as(
            "INSERT INTO clients (id, name, credential_hash, status, version, platform) VALUES ($1,$2,$3,'active',$4,$5)
             ON CONFLICT (name) DO UPDATE SET credential_hash=$3, status='active', version=$4, platform=$5, last_seen_at=now()
             RETURNING id",
        )
        .bind(cid)
        .bind(if name.is_empty() { "client" } else { name })
        .bind(&cred_hash)
        .bind(version)
        .bind(platform)
        .fetch_one(&self.db)
        .await?;
        sqlx::query("UPDATE enroll_tokens SET used = used + 1 WHERE id = $1")
            .bind(tok_id)
            .execute(&self.db)
            .await?;
        Ok(Some((row_id.0.to_string(), device_cred)))
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
        // Dashboard freshness reads Postgres last_seen_at — refresh it from
        // heartbeats, throttled to one write per minute per client so the
        // write rate stays flat regardless of heartbeat interval.
        let first_in_window: Option<String> = redis::cmd("SET")
            .arg(self.rt.k(&format!("client:{client_id}:seen_write")))
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(60)
            .query_async(&mut c)
            .await
            .unwrap_or(None);
        if first_in_window.is_some() {
            let _ = sqlx::query("UPDATE clients SET last_seen_at = now() WHERE id = $1::uuid")
                .bind(client_id)
                .execute(&self.db)
                .await;
        }
        let _ = session_id;
        Ok(())
    }

    /// Process an inventory report: upsert deployments (Postgres reported side),
    /// Deployment ids tombstoned via desired.deployments[id].removed —
    /// reports for these must not resurrect runtime state or DB rows.
    async fn removed_dep_ids(&self, client_id: &str) -> Vec<String> {
        let desired_deps: Value = sqlx::query_scalar(
            "SELECT COALESCE(desired_config->'deployments','{}'::jsonb) FROM clients WHERE id=$1::uuid",
        )
        .bind(client_id)
        .fetch_optional(&self.db)
        .await
        .unwrap_or_default()
        .unwrap_or_else(|| json!({}));
        desired_deps
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(_, v)| v["removed"].as_bool() == Some(true))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// update runtime dep hashes (Redis).
    pub async fn apply_inventory(&self, client_id: &str, inv: &InventoryReport) -> Result<()> {
        let mut c = self.rt.conn.clone();
        // Tombstones: deployments the admin hard-deleted carry
        // desired.deployments[id].removed — a report that still contains
        // one (sent before the ConfigPush landed) must not re-create the
        // row, and any row that slipped through gets deleted here.
        let removed_ids = self.removed_dep_ids(client_id).await;
        if !removed_ids.is_empty() {
            let _ = sqlx::query("DELETE FROM deployments WHERE id::text = ANY($1)")
                .bind(&removed_ids)
                .execute(&self.db)
                .await;
        }
        for d in &inv.deployments {
            if removed_ids.iter().any(|r| r == &d.deployment_id) {
                let _: () = redis::cmd("DEL")
                    .arg(self.rt.k(&format!("dep:{}", d.deployment_id)))
                    .arg(self.rt.k(&format!("dep:{}:leases", d.deployment_id)))
                    .query_async(&mut c)
                    .await
                    .unwrap_or(());
                continue;
            }
            upsert_deployment(&self.db, client_id, d).await?;
            write_dep_runtime(&mut c, &self.rt, client_id, d, None).await?;
            // Patch the routing snapshot: without this, fresh probe results
            // and enabled toggles only exist in Postgres/Redis while the
            // scheduler keeps filtering on the stale snapshot.
            let caps = d
                .capabilities
                .as_ref()
                .map(|cp| serde_json::from_value(dep_caps_json(cp)).unwrap_or_default());
            self.models
                .patch_deployment(&d.deployment_id, caps, Some(d.enabled))
                .await;
        }
        // Deployments no longer reported by the client → disable them so they
        // leave the routing pool (soft-delete: usage/history keep FK refs).
        let reported: Vec<String> = inv
            .deployments
            .iter()
            .map(|d| d.deployment_id.clone())
            .collect();
        let stale: Vec<String> = sqlx::query_scalar(
            "UPDATE deployments SET enabled=false, updated_at=now()
             WHERE client_id=$1::uuid AND enabled=true AND NOT (id::text = ANY($2))
             RETURNING id::text",
        )
        .bind(client_id)
        .bind(&reported)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for dep_id in &stale {
            let _: () = redis::cmd("HSET")
                .arg(self.rt.k(&format!("dep:{dep_id}")))
                .arg("state")
                .arg("unavailable")
                .query_async(&mut c)
                .await
                .unwrap_or(());
            self.models
                .patch_deployment(dep_id, None, Some(false))
                .await;
        }
        let caps: Vec<_> = inv
            .capacities
            .iter()
            .filter(|c| !removed_ids.iter().any(|r| r == &c.deployment_id))
            .cloned()
            .collect();
        self.apply_capacity(client_id, &caps, None).await?;
        Ok(())
    }

    /// Update runtime capacity hashes.
    pub async fn apply_capacity(
        &self,
        client_id: &str,
        caps: &[DeploymentCapacity],
        host: Option<&loom_protocol::pb::HostStats>,
    ) -> Result<()> {
        let mut c = self.rt.conn.clone();
        let removed_ids = self.removed_dep_ids(client_id).await;
        if let Some(h) = host {
            let _: () = redis::cmd("HSET")
                .arg(self.rt.k(&format!("client:{client_id}")))
                .arg("cpu_pct")
                .arg(h.cpu_pct)
                .arg("ram_pct")
                .arg(h.ram_pct)
                .arg("ram_used_bytes")
                .arg(h.ram_used_bytes)
                .arg("ram_total_bytes")
                .arg(h.ram_total_bytes)
                .arg("pressure")
                .arg(h.pressure as u32)
                .arg("has_gpu")
                .arg(h.has_gpu as u32)
                .arg("gpu_name")
                .arg(&h.gpu_name)
                .arg("gpu_util_pct")
                .arg(h.gpu_util_pct)
                .arg("gpu_mem_used_bytes")
                .arg(h.gpu_mem_used_bytes)
                .arg("gpu_mem_total_bytes")
                .arg(h.gpu_mem_total_bytes)
                .arg("stats_at")
                .arg(now_ms())
                .query_async(&mut c)
                .await?;
        }
        for cap in caps {
            if removed_ids.iter().any(|r| r == &cap.deployment_id) {
                continue;
            }
            let key = self.rt.k(&format!("dep:{}", cap.deployment_id));
            let state = if cap.accepting {
                "available"
            } else {
                "draining"
            };
            let mut h = redis::cmd("HSET");
            h.arg(&key)
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
                .arg("pressure_gated")
                .arg(cap.pressure_gated as u32)
                .arg("last_report")
                .arg(now_ms());
            // Newer clients report effective health per capacity tick — the
            // scheduler needs "down" here, not just the (staler) inventory
            // value, or a dead-but-draining member looks "busy" forever.
            // Older clients send "" → keep the inventory-written value.
            if !cap.health.is_empty() {
                h.arg("health").arg(&cap.health);
            }
            let _: () = h.query_async(&mut c).await?;
        }
        // wake the scheduler
        self.rt.publish("ev:capacity", "1").await.ok();
        Ok(())
    }

    /// Client went offline: run CLIENT_OFFLINE lua across its dep keys,
    /// return (requeue_request_ids, fail_request_ids).
    /// Mark a client offline and reap its leases — but only when the
    /// disconnecting stream is still the current session. A stale duplicate
    /// connection dying after a fresh Hello must not touch the live session's
    /// online flag, dep states or leases.
    pub async fn client_offline(
        &self,
        client_id: &str,
        session_id: &str,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut c = self.rt.conn.clone();
        let current: Option<String> = redis::cmd("HGET")
            .arg(self.rt.k(&format!("client:{client_id}")))
            .arg("session_id")
            .query_async(&mut c)
            .await
            .unwrap_or(None);
        if current.as_deref() != Some(session_id) {
            return Ok((vec![], vec![]));
        }
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
            health: m.get("health").cloned().unwrap_or_default(),
            tier2_free: m
                .get("tier2_free")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            tier2_total: m
                .get("tier2_total")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            client_id: m.get("client_id").cloned().unwrap_or_default(),
            pressure_gated: m.get("pressure_gated").map(|v| v == "1").unwrap_or(false),
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
                if rt.state == "available" && rt.tier2_free > 0 && rt.health != "down" {
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
            let vals: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(self.rt.k(&format!("client:{cid}")))
                .arg("online")
                .arg("cpu_pct")
                .arg("ram_pct")
                .arg("ram_used_bytes")
                .arg("ram_total_bytes")
                .arg("has_gpu")
                .arg("gpu_name")
                .arg("gpu_util_pct")
                .arg("gpu_mem_used_bytes")
                .arg("gpu_mem_total_bytes")
                .arg("stats_at")
                .arg("last_hb")
                .query_async(&mut c)
                .await
                .unwrap_or_default();
            let f = |i: usize| vals.get(i).and_then(|v| v.clone());
            let num = |i: usize| f(i).and_then(|s| s.parse::<f64>().ok());
            // Effective liveness: online flag AND a heartbeat younger than
            // the watcher's stale threshold (45s) — a crashed client can
            // leave online=1 behind long after its stream died.
            let hb_fresh = num(11)
                .map(|t| crate::client_registry::now_ms() - t as i64 <= 45_000)
                .unwrap_or(false);
            let live = f(0).as_deref() == Some("1") && hb_fresh;
            out.push(json!({
                "id": cid,
                "name": r.name,
                "status": if live { r.status.as_str() } else { "offline" },
                "enrollment_status": r.status,
                "online": live,
                "last_seen_at": r.last_seen_at,
                "version": r.version,
                "platform": r.platform,
                "labels": r.labels,
                "config_version": r.config_version,
                "stats": if f(10).is_some() { json!({
                    "cpu_pct": num(1),
                    "ram_pct": num(2),
                    "ram_used_bytes": num(3),
                    "ram_total_bytes": num(4),
                    "gpu": if f(5).as_deref() == Some("1") { json!({
                        "name": f(6),
                        "util_pct": num(7),
                        "mem_used_bytes": num(8),
                        "mem_total_bytes": num(9),
                    }) } else { serde_json::Value::Null },
                    "at": num(10),
                }) } else { serde_json::Value::Null },
            }));
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct DepRuntime {
    pub state: String,
    /// Client-reported generation health: "healthy" | "degraded" | "down" |
    /// "unknown". `down` (crash breaker open, or a dead backend) must be
    /// excluded from routing even while `state` still reads available.
    pub health: String,
    pub tier2_free: u32,
    pub tier2_total: u32,
    pub client_id: String,
    /// Closed by the client's local pressure monitor (cpu/ram over limits).
    pub pressure_gated: bool,
}

/// pb::Capabilities → the JSON shape stored in deployments.capabilities and
/// parsed into loom_core::Capabilities for the routing snapshot.
fn dep_caps_json(c: &loom_protocol::pb::Capabilities) -> serde_json::Value {
    json!({
        "chat": c.chat, "completions": c.completions, "embeddings": c.embeddings,
        "tools": c.tools, "vision": c.vision, "json_mode": c.json_mode,
        "streaming": c.streaming, "context_length": c.context_length,
        "modalities": c.modalities,
        "images": c.images, "audio_speech": c.audio_speech,
        "audio_transcription": c.audio_transcription,
        "audio_translation": c.audio_translation, "video": c.video,
    })
}

async fn upsert_deployment(db: &PgPool, client_id: &str, d: &DeploymentReport) -> Result<()> {
    let caps = d.capabilities.as_ref().map(dep_caps_json);
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
    // capabilities JSON for routing filter + admin views — keep in sync
    // with the full shape written to Postgres.
    if let Some(cp) = &d.capabilities {
        pipe.arg("capabilities")
            .arg(serde_json::to_string(&dep_caps_json(cp)).unwrap_or_default());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON written to deployments.capabilities / Redis must round-trip
    /// into loom_core::Capabilities — the scheduler filters on that shape.
    #[test]
    fn dep_caps_json_roundtrips_into_core_capabilities() {
        let pb = loom_protocol::pb::Capabilities {
            chat: true,
            vision: true,
            streaming: true,
            context_length: 262144,
            modalities: vec!["image".into()],
            images: true,
            audio_speech: true,
            video: true,
            ..Default::default()
        };
        let caps: loom_core::capability::Capabilities =
            serde_json::from_value(dep_caps_json(&pb)).unwrap();
        assert!(caps.vision);
        assert!(caps.images && caps.audio_speech && caps.video);
        assert_eq!(caps.context_length, 262144);
        assert!(caps.modalities.contains("image"));
    }
}
