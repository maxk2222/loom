//! loom CLI: enroll, status, diagnostics, db-migrate, create-admin, issue-enroll-token.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "loom", version, about = "LOOM platform CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enroll this workstation at a server (client-side).
    Enroll {
        #[arg(long)] server: String,
        #[arg(long)] token: String,
        #[arg(long, default_value = "")] name: String,
        #[arg(long, env = "LOOM_CONFIG", default_value = "")]
        config: String,
    },
    /// Show local client status (queries local admin API).
    Status {
        #[arg(long, default_value = "http://127.0.0.1:8391")]
        local: String,
    },
    /// Run database migrations (server-side).
    DbMigrate {
        #[arg(long, env = "LOOM_DATABASE_URL")]
        database_url: String,
    },
    /// Create an admin user (server-side, direct DB).
    CreateAdmin {
        #[arg(long, env = "LOOM_DATABASE_URL")]
        database_url: String,
        #[arg(long)] email: String,
        #[arg(long)] password: String,
        #[arg(long, default_value = "Admin")] name: String,
    },
    /// Issue an enrollment token (server-side, direct DB).
    IssueEnrollToken {
        #[arg(long, env = "LOOM_DATABASE_URL")]
        database_url: String,
        #[arg(long, default_value = "")] label: String,
        #[arg(long, default_value_t = 1)] max_uses: i32,
        #[arg(long, default_value_t = 24)] expires_hours: i64,
    },
    /// Print a diagnostics bundle (config paths, versions).
    Diagnostics,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "warn".into()),
    ).init();
    match Args::parse().cmd {
        Cmd::Enroll { server, token, name, config } => {
            let path = if config.is_empty() {
                loom_core::config::ClientConfig::default();
                default_config_path()
            } else { config.into() };
            let mut cfg = if path.exists() {
                toml::from_str::<loom_core::config::ClientConfig>(
                    &std::fs::read_to_string(&path)?)?
            } else {
                loom_core::config::ClientConfig::default()
            };
            cfg.server_url = server.clone();
            if !name.is_empty() {
                cfg.name = name;
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
            println!("enrolled: server={server} config={}", path.display());
            println!("start the client with LOOM_ENROLL_TOKEN={token} (first connect registers the device credential)");
            Ok(())
        }
        Cmd::Status { local } => {
            let v: serde_json::Value = reqwest::get(format!("{local}/local/api/status"))
                .await?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::DbMigrate { database_url } => {
            let pool = sqlx::PgPool::connect(&database_url).await?;
            sqlx::migrate!("../../migrations").run(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::CreateAdmin { database_url, email, password, name } => {
            let pool = sqlx::PgPool::connect(&database_url).await?;
            use argon2::password_hash::{rand_core::OsRng, SaltString};
            use argon2::{Argon2, PasswordHasher};
            let salt = SaltString::generate(&mut OsRng);
            let hash = Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .to_string();
            sqlx::query("INSERT INTO users (email,password_hash,display_name,role) VALUES ($1,$2,$3,'admin') ON CONFLICT (email) DO UPDATE SET password_hash=$2, role='admin'")
                .bind(&email).bind(&hash).bind(&name).execute(&pool).await?;
            println!("admin created: {email}");
            Ok(())
        }
        Cmd::IssueEnrollToken { database_url, label, max_uses, expires_hours } => {
            let pool = sqlx::PgPool::connect(&database_url).await?;
            let token = format!("enroll_{}", loom_core::ids::opaque_token(24));
            sqlx::query(
                "INSERT INTO enroll_tokens (token_hash,label,max_uses,expires_at) VALUES ($1,$2,$3, now() + ($4 || ' hours')::interval)")
                .bind(loom_core::ids::sha256_hex(&token))
                .bind(&label)
                .bind(max_uses)
                .bind(expires_hours.to_string())
                .execute(&pool).await?;
            println!("{token}");
            Ok(())
        }
        Cmd::Diagnostics => {
            println!("loom {}", env!("CARGO_PKG_VERSION"));
            println!("config: {}", std::env::var("LOOM_CONFIG").unwrap_or_else(|_| default_config_path().display().to_string()));
            println!("platform: {}-{}", std::env::consts::OS, std::env::consts::ARCH);
            Ok(())
        }
    }
}

fn default_config_path() -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(h) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(h)
                .join("Library/Application Support/loom-client/config.toml");
        }
    }
    "/etc/loom-client/config.toml".into()
}
