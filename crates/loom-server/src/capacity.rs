//! Fleet-capacity sampler: snapshots the scheduler's runtime view into
//! `capacity_samples` every ~20 s so dashboard charts can render long
//! windows. Raw gauges are persisted (never percentages) so historical
//! reads stay correct when budgets or configs change later.

use sqlx::PgPool;

use crate::client_registry::ClientRegistry;

const TICK_SECS: u64 = 20;
const CLEANUP_SECS: u64 = 3600;

/// Spawn the sampler loop. Best-effort: failures are logged and retried on
/// the next tick — a missing runtime hash (client offline) still records a
/// row, marked not-live, so gaps in fleet size stay visible.
pub fn spawn(db: PgPool, registry: ClientRegistry, retention_days: u32) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        let mut last_cleanup = std::time::Instant::now();
        loop {
            tick.tick().await;
            if let Err(e) = sample_once(&db, &registry).await {
                tracing::warn!(error = %e, "capacity sample failed");
            }
            if last_cleanup.elapsed() > std::time::Duration::from_secs(CLEANUP_SECS) {
                last_cleanup = std::time::Instant::now();
                if let Err(e) = sqlx::query(
                    "DELETE FROM capacity_samples WHERE ts < now() - ($1 || ' days')::interval",
                )
                .bind(retention_days.to_string())
                .execute(&db)
                .await
                {
                    tracing::warn!(error = %e, "capacity_samples cleanup failed");
                }
            }
        }
    });
}

struct Row {
    dep_id: uuid::Uuid,
    client_id: Option<uuid::Uuid>,
    kv_used: i64,
    kv_budget: i64,
    slots_total: i32,
    slots_free: i32,
    state: String,
    health: String,
    kv_gated: bool,
    live: bool,
}

async fn sample_once(db: &PgPool, registry: &ClientRegistry) -> anyhow::Result<()> {
    type Dep = (
        uuid::Uuid,
        Option<uuid::Uuid>,
        bool,
        String,
        Option<i64>,
        Option<i64>,
    );
    let deps: Vec<Dep> = sqlx::query_as(
        "SELECT id, client_id, enabled, admission_mode, kv_budget_tokens, reported_kv_budget_tokens
           FROM deployments",
    )
    .fetch_all(db)
    .await?;
    let mut rows = Vec::with_capacity(deps.len());
    for (dep_id, client_id, enabled, amode, kvb, rkvb) in deps {
        let rt = registry
            .dep_runtime(&dep_id.to_string())
            .await
            .ok()
            .flatten();
        let state = rt.as_ref().map(|r| r.state.clone()).unwrap_or_default();
        let health = rt.as_ref().map(|r| r.health.clone()).unwrap_or_default();
        // Same predicate the dashboard tile applies — frozen at sample time.
        let live = enabled && state == "available" && health != "down";
        let kv_budget = kvb.or(rkvb).unwrap_or(0);
        let kv_gated = live && amode != "slots" && kv_budget > 0;
        rows.push(Row {
            dep_id,
            client_id,
            kv_used: rt.as_ref().map(|r| r.kv_used).unwrap_or(0),
            kv_budget,
            slots_total: rt.as_ref().map(|r| r.tier2_total as i32).unwrap_or(0),
            slots_free: rt.as_ref().map(|r| r.tier2_free as i32).unwrap_or(0),
            state,
            health,
            kv_gated,
            live,
        });
    }
    if rows.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO capacity_samples
           (ts, deployment_id, client_id, kv_used, kv_budget, slots_total, slots_free,
            state, health, kv_gated, live)
         SELECT now(), * FROM UNNEST($1::uuid[], $2::uuid[], $3::bigint[], $4::bigint[],
                                     $5::int[], $6::int[], $7::text[], $8::text[],
                                     $9::bool[], $10::bool[])",
    )
    .bind(rows.iter().map(|r| r.dep_id).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.client_id).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.kv_used).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.kv_budget).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.slots_total).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.slots_free).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.state.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.health.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.kv_gated).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.live).collect::<Vec<_>>())
    .execute(db)
    .await?;
    Ok(())
}
