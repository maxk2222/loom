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
        #[arg(long)]
        server: String,
        #[arg(long)]
        token: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, env = "LOOM_CONFIG", default_value = "")]
        config: String,
    },
    /// Show local client status (queries local admin API).
    Status {
        #[arg(long, default_value = "http://127.0.0.1:8391")]
        local: String,
        /// Bearer key when the client has local_api.api_key configured.
        #[arg(long, default_value = "", env = "LOOM_LOCAL_API_KEY")]
        api_key: String,
    },
    /// Show or edit the client config file (client-side).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
        #[arg(long, env = "LOOM_CONFIG", default_value = "", global = true)]
        config: String,
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
        #[arg(long)]
        email: String,
        #[arg(long)]
        password: String,
        #[arg(long, default_value = "Admin")]
        name: String,
    },
    /// Issue an enrollment token (server-side, direct DB).
    IssueEnrollToken {
        #[arg(long, env = "LOOM_DATABASE_URL")]
        database_url: String,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long, default_value_t = 1)]
        max_uses: i32,
        #[arg(long, default_value_t = 24)]
        expires_hours: i64,
    },
    /// Print a diagnostics bundle (config paths, versions).
    Diagnostics,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective config (secrets masked).
    Show,
    /// Read a single key — e.g. `loom config get local-api.bind`.
    Get { key: String },
    /// Set a key — e.g. `loom config set local-api.bind 0.0.0.0:8391`.
    /// Bind/credential changes apply on client restart.
    Set { key: String, value: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    match Args::parse().cmd {
        Cmd::Enroll {
            server,
            token,
            name,
            config,
        } => {
            let path = if config.is_empty() {
                loom_core::config::ClientConfig::default();
                default_config_path()
            } else {
                config.into()
            };
            let mut cfg = if path.exists() {
                toml::from_str::<loom_core::config::ClientConfig>(&std::fs::read_to_string(&path)?)?
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
        Cmd::Status { local, api_key } => {
            let http = reqwest::Client::new();
            let mut req = http.get(format!("{local}/local/api/status"));
            if !api_key.is_empty() {
                req = req.bearer_auth(api_key);
            }
            let resp = req.send().await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "local API requires auth — pass --api-key (or set LOOM_LOCAL_API_KEY)"
                );
            }
            let v: serde_json::Value = resp.error_for_status()?.json().await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::Config { action, config } => {
            let path = if config.is_empty() {
                default_config_path()
            } else {
                config.into()
            };
            run_config_cmd(action, &path)
        }
        Cmd::DbMigrate { database_url } => {
            let pool = sqlx::PgPool::connect(&database_url).await?;
            sqlx::migrate!("../../migrations").run(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::CreateAdmin {
            database_url,
            email,
            password,
            name,
        } => {
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
        Cmd::IssueEnrollToken {
            database_url,
            label,
            max_uses,
            expires_hours,
        } => {
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
            println!(
                "config: {}",
                std::env::var("LOOM_CONFIG")
                    .unwrap_or_else(|_| default_config_path().display().to_string())
            );
            println!(
                "platform: {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
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

// --- loom config ------------------------------------------------------------

fn load_client_cfg(path: &std::path::Path) -> Result<loom_core::config::ClientConfig> {
    if path.exists() {
        Ok(toml::from_str::<loom_core::config::ClientConfig>(
            &std::fs::read_to_string(path)?,
        )?)
    } else {
        Ok(loom_core::config::ClientConfig::default())
    }
}

/// Editable keys: `kebab-case` and `snake_case` spellings both accepted.
/// Returns the current value; `set` writes `value` (bool keys parse
/// "true"/"false", everything else is a string).
fn cfg_access(
    cfg: &mut loom_core::config::ClientConfig,
    key: &str,
    set: Option<&str>,
) -> Result<String> {
    let k = key.replace('-', "_");
    let cur: String = match k.as_str() {
        "name" => string_or_set(&mut cfg.name, set),
        "server_url" => string_or_set(&mut cfg.server_url, set),
        "connect_enabled" => bool_or_set(&mut cfg.connect_enabled, set)?,
        "sharing_enabled" => bool_or_set(&mut cfg.sharing_enabled, set)?,
        "local_api.bind" => string_or_set(&mut cfg.local_api.bind, set),
        "local_api.api_key" => string_or_set(&mut cfg.local_api.api_key, set),
        "local_api.serve_ui" => bool_or_set(&mut cfg.local_api.serve_ui, set)?,
        "updater.enabled" => bool_or_set(&mut cfg.updater.enabled, set)?,
        "updater.channel" => string_or_set(&mut cfg.updater.channel, set),
        "updater.auto_apply" => bool_or_set(&mut cfg.updater.auto_apply, set)?,
        "tray.enabled" => bool_or_set(&mut cfg.tray.enabled, set)?,
        _ => anyhow::bail!(
            "unknown key {key:?} — editable keys: name, server-url, connect-enabled, \
             sharing-enabled, local-api.bind, local-api.api-key, local-api.serve-ui, \
             updater.enabled, updater.channel, updater.auto-apply, tray.enabled"
        ),
    };
    Ok(cur)
}

fn string_or_set(field: &mut String, set: Option<&str>) -> String {
    if let Some(v) = set {
        *field = v.to_string();
    }
    field.clone()
}

fn bool_or_set(field: &mut bool, set: Option<&str>) -> Result<String> {
    if let Some(v) = set {
        *field = match v {
            "true" => true,
            "false" => false,
            other => anyhow::bail!("expected true/false, got {other:?}"),
        };
    }
    Ok(field.to_string())
}

fn run_config_cmd(action: ConfigCmd, path: &std::path::Path) -> Result<()> {
    match action {
        ConfigCmd::Show => {
            let mut cfg = load_client_cfg(path)?;
            // Mask secrets — show is for humans.
            if !cfg.device_credential.is_empty() {
                cfg.device_credential = "***".into();
            }
            if !cfg.local_api.api_key.is_empty() {
                cfg.local_api.api_key = "***".into();
            }
            if !cfg.local_api.user_hash.is_empty() {
                cfg.local_api.user_hash = "***".into();
            }
            print!("{}", toml::to_string_pretty(&cfg)?);
        }
        ConfigCmd::Get { key } => {
            let mut cfg = load_client_cfg(path)?;
            println!("{}", cfg_access(&mut cfg, &key, None)?);
        }
        ConfigCmd::Set { key, value } => {
            let mut cfg = load_client_cfg(path)?;
            cfg_access(&mut cfg, &key, Some(&value))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, toml::to_string_pretty(&cfg)?)?;
            println!("{key} = {value}");
            if key.replace('-', "_").starts_with("local_api.")
                || key.replace('-', "_") == "server_url"
            {
                eprintln!("note: restart the client to apply (systemctl restart loom-client)");
            }
        }
    }
    Ok(())
}
