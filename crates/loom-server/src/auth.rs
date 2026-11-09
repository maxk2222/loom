//! Authentication: API-key authn for /v1/*, session-cookie authn for /api/*,
//! argon2 passwords, opaque tokens hashed with sha256.

use anyhow::Result;
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use loom_core::ids::{opaque_token, sha256_hex};
use serde::Deserialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Negative cache for invalid/revoked API keys: sha256 -> first failure
/// timestamp. A bad key stays bad for 10s — repeated sprays skip the DB
/// join instead of paying it per request. Key revocation can therefore
/// take up to 10s to propagate — acceptable vs. a background flush.
fn bad_key_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Failed-login throttle map: "ip|email" -> (failures, locked_until).
type LoginThrottle = Arc<Mutex<HashMap<String, (u32, Option<Instant>)>>>;

fn login_locked(m: &LoginThrottle, key: &str) -> bool {
    let now = Instant::now();
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() > 10_000 {
        g.retain(|_, (_, until)| until.is_some_and(|u| u > now));
    }
    matches!(g.get(key), Some((_, Some(until))) if *until > now)
}

fn record_login_failure(m: &LoginThrottle, key: &str) {
    let now = Instant::now();
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() > 10_000 {
        g.clear();
    }
    let fails = g.get(key).map(|(f, _)| *f).unwrap_or(0) + 1;
    let lock = if fails >= 5 {
        let secs = 30u64.saturating_mul(1u64 << (fails - 5).min(5)).min(900);
        Some(now + Duration::from_secs(secs))
    } else {
        None
    };
    g.insert(key.to_string(), (fails, lock));
}

#[derive(Clone)]
pub struct Auth {
    pub db: PgPool,
    pub rt: crate::redisx::Rt,
    /// Shared across clones so the limit cannot be bypassed via a cloned handle.
    pub login_throttle: LoginThrottle,
}

#[derive(Debug, Clone)]
pub struct ApiKeyCtx {
    pub api_key_id: String,
    pub project_id: String,
    pub allowed_models: Vec<String>,
    pub scopes: Vec<String>,
    pub rpm_limit: i64,
    pub tpm_limit: i64,
    pub max_concurrent: i64,
    pub priority_class: u32,
    pub weight: i64,
}

#[derive(Debug, Clone)]
pub struct UserCtx {
    pub user_id: String,
    pub role: String,
    pub email: String,
}

impl Auth {
    /// Validate an OpenAI API key; returns ctx with project + limits.
    pub async fn check_api_key(&self, token: &str) -> Result<Option<ApiKeyCtx>> {
        // Reject before hashing: a key row minted from an empty token would
        // otherwise authenticate any request with no Authorization header.
        if token.is_empty() {
            return Ok(None);
        }
        let hash = sha256_hex(token);
        // Short negative cache: key-spraying attempts re-hit this same
        // lookup otherwise — a miss stays a miss for 10s, keyed on the
        // already-hashed value so no plaintext token is stored.
        {
            let mut miss = bad_key_cache().lock().unwrap();
            let now = std::time::Instant::now();
            if miss.len() > 10_000 {
                miss.retain(|_, t| now.duration_since(*t).as_secs() < 10);
            }
            if let Some(t) = miss.get(&hash) {
                if now.duration_since(*t).as_secs() < 10 {
                    return Ok(None);
                }
            }
        }
        #[derive(sqlx::FromRow)]
        struct Row {
            id: uuid::Uuid,
            project_id: uuid::Uuid,
            scopes: serde_json::Value,
            rpm_limit: i32,
            tpm_limit: i32,
            max_concurrent: i32,
            allowed_models: serde_json::Value,
            priority_class: i32,
            weight: i32,
            project_enabled: bool,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT k.id, k.project_id, k.scopes, k.rpm_limit, k.tpm_limit,
                    k.max_concurrent, k.allowed_models,
                    p.priority_class, p.weight, p.enabled AS project_enabled
             FROM api_keys k JOIN projects p ON p.id = k.project_id
             WHERE k.key_hash = $1 AND k.revoked_at IS NULL
               AND (k.expires_at IS NULL OR k.expires_at > now())",
        )
        .bind(&hash)
        .fetch_optional(&self.db)
        .await?;
        let Some(r) = row else {
            bad_key_cache()
                .lock()
                .unwrap()
                .insert(hash, std::time::Instant::now());
            return Ok(None);
        };
        if !r.project_enabled {
            bad_key_cache()
                .lock()
                .unwrap()
                .insert(hash, std::time::Instant::now());
            return Ok(None);
        }
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
            .bind(r.id)
            .execute(&self.db)
            .await;
        Ok(Some(ApiKeyCtx {
            api_key_id: r.id.to_string(),
            project_id: r.project_id.to_string(),
            allowed_models: serde_json::from_value(r.allowed_models).unwrap_or_default(),
            scopes: serde_json::from_value(r.scopes).unwrap_or_default(),
            rpm_limit: r.rpm_limit as i64,
            tpm_limit: r.tpm_limit as i64,
            max_concurrent: r.max_concurrent as i64,
            priority_class: r.priority_class.max(0) as u32,
            weight: r.weight as i64,
        }))
    }

    /// Model access: [] = project grants via project_models, ["*"] = all.
    /// Entries may be virtual-model ids (UUIDs, as stored by the admin UI) or
    /// api_names — both are accepted.
    pub async fn model_allowed(&self, ctx: &ApiKeyCtx, vm_api_name: &str) -> Result<bool> {
        if ctx.allowed_models.iter().any(|m| m == "*") {
            return Ok(true);
        }
        // Resolve once: needed for id-entries in allowed_models and for the
        // project-grant fallback.
        let vm_id: Option<String> = sqlx::query_scalar(
            "SELECT id::text FROM virtual_models WHERE api_name = $1 AND enabled = true",
        )
        .bind(vm_api_name)
        .fetch_optional(&self.db)
        .await?;
        let Some(vm_id) = vm_id else { return Ok(false) };
        if !ctx.allowed_models.is_empty() {
            return Ok(ctx
                .allowed_models
                .iter()
                .any(|m| m == vm_api_name || m == &vm_id));
        }
        // project grant check
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM project_models WHERE project_id = $1::uuid AND virtual_model_id = $2::uuid",
        )
        .bind(&ctx.project_id)
        .bind(&vm_id)
        .fetch_one(&self.db)
        .await?;
        Ok(n > 0)
    }

    /// Rate limit + concurrency check via Lua; commit on success.
    pub async fn rl_check(&self, ctx: &ApiKeyCtx, request_id: &str) -> Result<(), &'static str> {
        let mut c = self.rt.conn.clone();
        let now = now_ms();
        let res: String = redis::Script::new(crate::lua::RL_CHECK)
            .key(self.rt.k(&format!("rl:key:{}:rpm", ctx.api_key_id)))
            .key(self.rt.k(&format!("inflight:key:{}", ctx.api_key_id)))
            .key(self.rt.k(&format!("rl:key:{}:tpm", ctx.api_key_id)))
            .arg(now)
            .arg(60_000)
            .arg(ctx.rpm_limit)
            .arg(ctx.max_concurrent)
            .arg(ctx.tpm_limit)
            .invoke_async(&mut c)
            .await
            .map_err(|_| "redis")?;
        match res.as_str() {
            "OK" => {
                let _: String = redis::Script::new(crate::lua::RL_COMMIT)
                    .key(self.rt.k(&format!("rl:key:{}:rpm", ctx.api_key_id)))
                    .key(self.rt.k(&format!("inflight:key:{}", ctx.api_key_id)))
                    .arg(now)
                    .arg(request_id)
                    .arg(60_000)
                    .invoke_async(&mut c)
                    .await
                    .unwrap_or_default();
                Ok(())
            }
            "RPM" => Err("rate_limited"),
            "TPM" => Err("tpm_limited"),
            "CONCURRENCY" => Err("concurrency_exceeded"),
            _ => Err("rate_limited"),
        }
    }

    /// Charge a completed request's token spend into the key's 60s sliding
    /// TPM window. Called by the request finalizer once real usage is known.
    pub async fn rl_charge_tpm(&self, api_key_id: &str, request_id: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let mut c = self.rt.conn.clone();
        let _: String = redis::Script::new(crate::lua::TPM_CHARGE)
            .key(self.rt.k(&format!("rl:key:{api_key_id}:tpm")))
            .arg(now_ms())
            .arg(format!("{request_id}:{tokens}"))
            .arg(60_000)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();
    }

    pub async fn rl_release(&self, api_key_id: &str) {
        let mut c = self.rt.conn.clone();
        let _: i64 = redis::Script::new(crate::lua::INFLIGHT_DEC)
            .key(self.rt.k(&format!("inflight:key:{api_key_id}")))
            .invoke_async(&mut c)
            .await
            .unwrap_or(0);
    }

    // ---- dashboard user auth ----

    pub fn hash_password(pw: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        Ok(Argon2::default()
            .hash_password(pw.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_string())
    }

    pub fn verify_password(hash: &str, pw: &str) -> bool {
        let parsed = match PasswordHash::new(hash) {
            Ok(h) => h,
            Err(_) => return false,
        };
        Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok()
    }

    fn login_locked(&self, key: &str) -> bool {
        login_locked(&self.login_throttle, key)
    }

    fn record_login_failure(&self, key: &str) {
        record_login_failure(&self.login_throttle, key)
    }

    /// Login: check email/password → create session token.
    /// Failed attempts are throttled per (ip, email): ≥5 failures trigger an
    /// exponentially growing lockout (30s → 15min cap) to blunt brute force.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        ip: &str,
    ) -> Result<Option<(String, UserCtx)>> {
        let throttle_key = format!("{ip}|{}", email.to_ascii_lowercase());
        if self.login_locked(&throttle_key) {
            return Ok(None);
        }
        #[derive(sqlx::FromRow)]
        struct U {
            id: uuid::Uuid,
            password_hash: String,
            role: String,
            email: String,
            disabled: bool,
        }
        let u: Option<U> = sqlx::query_as(
            "SELECT id, password_hash, role, email, disabled FROM users WHERE email = $1",
        )
        .bind(email)
        .fetch_optional(&self.db)
        .await?;
        let Some(u) = u else {
            self.record_login_failure(&throttle_key);
            return Ok(None);
        };
        if u.disabled || !Self::verify_password(&u.password_hash, password) {
            self.record_login_failure(&throttle_key);
            return Ok(None);
        }
        self.login_throttle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&throttle_key);
        let token = opaque_token(32);
        let th = sha256_hex(&token);
        sqlx::query(
            "INSERT INTO sessions (user_id, token_hash, expires_at, ip) VALUES ($1,$2, now() + interval '7 days', $3)",
        )
        .bind(u.id)
        .bind(&th)
        .bind(ip)
        .execute(&self.db)
        .await?;
        Ok(Some((
            token,
            UserCtx {
                user_id: u.id.to_string(),
                role: u.role,
                email: u.email,
            },
        )))
    }

    /// Resolve a session cookie → user ctx.
    pub async fn user_for_token(&self, token: &str) -> Result<Option<UserCtx>> {
        if token.is_empty() {
            return Ok(None);
        }
        let th = sha256_hex(token);
        #[derive(sqlx::FromRow)]
        struct SessRow {
            user_id: uuid::Uuid,
            role: String,
            email: String,
            disabled: bool,
        }
        let r: Option<SessRow> = sqlx::query_as(
            "SELECT u.id AS user_id, u.role, u.email, u.disabled
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(&th)
        .fetch_optional(&self.db)
        .await?;
        Ok(r.and_then(|r| {
            (!r.disabled).then(|| UserCtx {
                user_id: r.user_id.to_string(),
                role: r.role,
                email: r.email,
            })
        }))
    }

    pub async fn logout(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(sha256_hex(token))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn admin_count(&self) -> Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'admin'")
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Create the first admin (setup wizard) — only when none exists.
    pub async fn create_first_admin(
        &self,
        email: &str,
        password: &str,
        name: &str,
    ) -> Result<bool> {
        if self.admin_count().await? > 0 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO users (email, password_hash, display_name, role) VALUES ($1,$2,$3,'admin')",
        )
        .bind(email)
        .bind(Self::hash_password(password)?)
        .bind(name)
        .execute(&self.db)
        .await?;
        Ok(true)
    }
}

fn now_ms() -> i64 {
    crate::client_registry::now_ms()
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_locks_after_five_failures() {
        let m: LoginThrottle = Default::default();
        let key = "1.2.3.4|a@b.c";
        for i in 1..=4 {
            assert!(!login_locked(&m, key), "locked too early at {i}");
            record_login_failure(&m, key);
        }
        assert!(!login_locked(&m, key));
        record_login_failure(&m, key); // 5th failure → locked
        assert!(login_locked(&m, key));
        // Different key (other ip or email) unaffected.
        assert!(!login_locked(&m, "5.6.7.8|a@b.c"));
        assert!(!login_locked(&m, "1.2.3.4|x@y.z"));
    }

    #[test]
    fn throttle_backoff_grows_and_caps() {
        let m: LoginThrottle = Default::default();
        let key = "k";
        for _ in 0..5 {
            record_login_failure(&m, key);
        }
        let until5 = m.lock().unwrap().get(key).unwrap().1.unwrap();
        for _ in 0..20 {
            record_login_failure(&m, key);
        }
        let until25 = m.lock().unwrap().get(key).unwrap().1.unwrap();
        // 15min cap: even after many failures lock stays ≤ 15min out.
        assert!(until25.duration_since(Instant::now()) <= Duration::from_secs(901));
        assert!(until25 > until5);
    }
}
