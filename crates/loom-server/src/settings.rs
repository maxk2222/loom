//! Settings + branding: server-wide JSONB settings, public branding endpoint,
//! logo upload to data dir.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;

#[derive(Clone)]
pub struct Settings {
    pub db: PgPool,
    pub data_dir: String,
}

pub const KEY_BRANDING: &str = "branding";
pub const KEY_SCHEDULER: &str = "scheduler";
pub const KEY_QUEUE: &str = "queue";

impl Settings {
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        let v: Option<Value> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        Ok(v)
    }

    pub async fn set(&self, key: &str, value: Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1,$2)
             ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()",
        )
        .bind(key)
        .bind(&value)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn branding(&self) -> Result<Value> {
        Ok(self.get(KEY_BRANDING).await?.unwrap_or_else(|| {
            json!({
                "app_name": "LOOM",
                "tagline": "LLM Orchestration & Operations Mesh",
                "logo_path": "",
                "accent_color": "#5b8def",
            })
        }))
    }

    /// Save a logo file into the data dir; returns public path.
    pub async fn save_logo(&self, ext: &str, bytes: &[u8]) -> Result<String> {
        let allowed = ["png", "jpg", "jpeg", "webp"];
        if !allowed.contains(&ext.to_lowercase().as_str()) {
            anyhow::bail!("unsupported logo type");
        }
        if bytes.len() > 2 * 1024 * 1024 {
            anyhow::bail!("logo too large");
        }
        let dir = format!("{}/branding", self.data_dir);
        tokio::fs::create_dir_all(&dir).await?;
        let path = format!("{dir}/logo.{ext}");
        tokio::fs::write(&path, bytes).await?;
        let mut b = self.branding().await?;
        b["logo_path"] = json!(format!("/api/branding/logo.{ext}"));
        self.set(KEY_BRANDING, b).await?;
        Ok(path)
    }

    pub async fn logo_path(&self) -> Result<Option<String>> {
        let b = self.branding().await?;
        Ok(b.get("logo_path").and_then(|v| v.as_str()).map(|s| {
            format!("{}/branding/{}", self.data_dir, s.rsplit('/').next().unwrap_or(""))
        }))
    }
}
