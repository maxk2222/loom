//! Redis runtime layer: namespaced keys + typed helpers. All scheduling
//! mutations go through Lua scripts (see `lua.rs`).

use anyhow::Result;
use redis::aio::ConnectionManager;
use redis::{Client, RedisError};

#[derive(Clone)]
pub struct Rt {
    pub conn: ConnectionManager,
    pub prefix: String,
}

impl Rt {
    pub async fn connect(url: &str, prefix: &str) -> Result<Self> {
        let client = Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            prefix: prefix.to_string(),
        })
    }

    pub fn k(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// Boot reconciliation: wipe all runtime keys under the prefix.
    /// Called once at server start — no persistent data lives in Redis.
    /// `instance:owner` is deliberately preserved: the owner lock is acquired
    /// BEFORE this wipe, and deleting it here would hand ownership to any
    /// racing peer.
    pub async fn reset_runtime(&self) -> Result<()> {
        let mut conn = self.conn.clone();
        let pattern = format!("{}*", self.prefix);
        let owner_key = self.k("instance:owner");
        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await?;
            let keys: Vec<String> = keys.into_iter().filter(|k| k != &owner_key).collect();
            if !keys.is_empty() {
                let _: () = redis::pipe()
                    .cmd("DEL")
                    .arg(&keys)
                    .ignore()
                    .query_async(&mut conn)
                    .await?;
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        Ok(())
    }

    pub async fn publish(&self, channel: &str, msg: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = redis::cmd("PUBLISH")
            .arg(self.k(channel))
            .arg(msg)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }
}

impl From<RedisError> for crate::scheduler::SchedErr {
    fn from(e: RedisError) -> Self {
        crate::scheduler::SchedErr::Backend(e.to_string())
    }
}
