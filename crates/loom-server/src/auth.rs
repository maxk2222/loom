//! Authentication: API-key authn for /v1/*, session-cookie authn for /api/*,
//! argon2 passwords, opaque tokens hashed with sha256.

use anyhow::Result;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use loom_core::ids::{opaque_token, sha256_hex};
use serde::Deserialize;
use sqlx::PgPool;

#[derive(Clone)]
pub struct Auth {
    pub db: PgPool,
    pub rt: crate::redisx::Rt,
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
        let hash = sha256_hex(token);
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
        let Some(r) = row else { return Ok(None) };
        if !r.project_enabled {
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
    pub async fn model_allowed(&self, ctx: &ApiKeyCtx, vm_api_name: &str) -> Result<bool> {
        if ctx.allowed_models.iter().any(|m| m == "*") {
            return Ok(true);
        }
        if !ctx.allowed_models.is_empty() {
            return Ok(ctx.allowed_models.iter().any(|m| m == vm_api_name));
        }
        // project grant check
        let vm_id: Option<String> = sqlx::query_scalar(
            "SELECT id::text FROM virtual_models WHERE api_name = $1 AND enabled = true",
        )
        .bind(vm_api_name)
        .fetch_optional(&self.db)
        .await?;
        let Some(vm_id) = vm_id else { return Ok(false) };
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
            .arg(now)
            .arg(60_000)
            .arg(ctx.rpm_limit)
            .arg(ctx.max_concurrent)
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
            "CONCURRENCY" => Err("concurrency_exceeded"),
            _ => Err("rate_limited"),
        }
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

    /// Login: check email/password → create session token.
    pub async fn login(&self, email: &str, password: &str, ip: &str) -> Result<Option<(String, UserCtx)>> {
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
        let Some(u) = u else { return Ok(None) };
        if u.disabled || !Self::verify_password(&u.password_hash, password) {
            return Ok(None);
        }
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
    pub async fn create_first_admin(&self, email: &str, password: &str, name: &str) -> Result<bool> {
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
