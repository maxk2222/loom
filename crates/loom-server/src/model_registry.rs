//! Model registry: runtime snapshot of virtual models + members + deployments,
//! built from Postgres, invalidated on config events.

use anyhow::Result;
use loom_core::capability::{ApiType, Capabilities};
use loom_core::RequiredCaps;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct VirtualModel {
    pub id: String,
    pub api_name: String,
    pub api_type: ApiType,
    pub routing_strategy: String,
    pub required_caps: RequiredCaps,
    pub allow_heterogeneous: bool,
    pub fallback_virtual_model_id: Option<String>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub deployment_id: String,
    pub client_id: String,
    pub model_name: String,
    pub capabilities: Capabilities,
    pub priority: i64,
    pub weight: i64,
    pub manual_pinned: bool,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct ModelRegistry {
    db: PgPool,
    snapshot: Arc<RwLock<HashMap<String, VirtualModel>>>, // keyed by api_name
    by_id: Arc<RwLock<HashMap<String, String>>>,          // vm_id -> api_name
}

impl ModelRegistry {
    pub fn new(db: PgPool) -> Self {
        Self {
            db,
            snapshot: Arc::new(RwLock::new(HashMap::new())),
            by_id: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Rebuild the in-memory snapshot from Postgres.
    pub async fn reload(&self) -> Result<()> {
        #[derive(sqlx::FromRow)]
        struct VmRow {
            id: uuid::Uuid,
            api_name: String,
            api_type: String,
            routing_strategy: String,
            required_capabilities: serde_json::Value,
            allow_heterogeneous: bool,
            fallback_virtual_model_id: Option<uuid::Uuid>,
        }
        #[derive(sqlx::FromRow)]
        struct MRow {
            virtual_model_id: uuid::Uuid,
            deployment_id: uuid::Uuid,
            client_id: uuid::Uuid,
            model_name: String,
            capabilities: serde_json::Value,
            priority: i32,
            weight: i32,
            manual_pinned: bool,
            enabled: bool,
        }

        let vms: Vec<VmRow> = sqlx::query_as(
            "SELECT id, api_name, api_type, routing_strategy, required_capabilities,
                    allow_heterogeneous, fallback_virtual_model_id
             FROM virtual_models WHERE enabled = true",
        )
        .fetch_all(&self.db)
        .await?;

        let members: Vec<MRow> = sqlx::query_as(
            "SELECT m.virtual_model_id, m.deployment_id, d.client_id, d.model_name,
                    d.capabilities, m.priority, m.weight, m.manual_pinned,
                    (m.enabled AND d.enabled) AS enabled
             FROM virtual_model_members m
             JOIN deployments d ON d.id = m.deployment_id",
        )
        .fetch_all(&self.db)
        .await?;

        let mut by_member: HashMap<String, Vec<Member>> = HashMap::new();
        for m in members {
            let caps: Capabilities =
                serde_json::from_value(m.capabilities).unwrap_or_default();
            by_member
                .entry(m.virtual_model_id.to_string())
                .or_default()
                .push(Member {
                    deployment_id: m.deployment_id.to_string(),
                    client_id: m.client_id.to_string(),
                    model_name: m.model_name,
                    capabilities: caps,
                    priority: m.priority as i64,
                    weight: m.weight as i64,
                    manual_pinned: m.manual_pinned,
                    enabled: m.enabled,
                });
        }

        let mut snap = HashMap::new();
        let mut by_id = HashMap::new();
        for v in vms {
            let id = v.id.to_string();
            let req_caps: RequiredCaps =
                serde_json::from_value(v.required_capabilities).unwrap_or_default();
            let vm = VirtualModel {
                api_name: v.api_name.clone(),
                api_type: v.api_type.parse().unwrap_or(ApiType::Chat),
                routing_strategy: v.routing_strategy,
                required_caps: req_caps,
                allow_heterogeneous: v.allow_heterogeneous,
                fallback_virtual_model_id: v.fallback_virtual_model_id.map(|u| u.to_string()),
                members: by_member.remove(&id).unwrap_or_default(),
                id: id.clone(),
            };
            by_id.insert(id, v.api_name.clone());
            snap.insert(v.api_name, vm);
        }
        *self.snapshot.write().await = snap;
        *self.by_id.write().await = by_id;
        Ok(())
    }

    /// Resolve an API model name to its virtual model.
    pub async fn resolve(&self, api_name: &str) -> Option<VirtualModel> {
        self.snapshot.read().await.get(api_name).cloned()
    }

    pub async fn resolve_by_id(&self, vm_id: &str) -> Option<VirtualModel> {
        let name = self.by_id.read().await.get(vm_id).cloned()?;
        self.resolve(&name).await
    }

    /// List enabled virtual model api names (for /v1/models).
    pub async fn list(&self) -> Vec<String> {
        self.snapshot.read().await.keys().cloned().collect()
    }

    /// Candidate members for a request, filtered by api type + required caps.
    pub async fn candidates(
        &self,
        vm: &VirtualModel,
        api: ApiType,
        req: &RequiredCaps,
    ) -> Vec<Member> {
        let merged = merge_caps(&vm.required_caps, req);
        let mut out: Vec<Member> = vm
            .members
            .iter()
            .filter(|m| {
                if !m.enabled {
                    return false;
                }
                // Unknown capabilities (never probed, reported as all-false) are
                // permissive for the api-type filter — the client's admission
                // control remains authoritative; required extras still enforced.
                let unknown = m.capabilities == Capabilities::default();
                (unknown || m.capabilities.supports_api(api))
                    && (unknown && merged == RequiredCaps::default()
                        || m.capabilities.satisfies(&merged))
            })
            .cloned()
            .collect();
        // If VM disallows heterogeneity, members must share one model_name.
        if !vm.allow_heterogeneous && out.len() > 1 {
            if let Some(first) = out.first().map(|m| m.model_name.clone()) {
                out.retain(|m| m.model_name == first);
            }
        }
        out
    }

    /// Follow the fallback chain (one hop, explicit only).
    pub async fn fallback(&self, vm: &VirtualModel) -> Option<VirtualModel> {
        let id = vm.fallback_virtual_model_id.as_ref()?;
        self.resolve_by_id(id).await
    }
}

fn merge_caps(a: &RequiredCaps, b: &RequiredCaps) -> RequiredCaps {
    RequiredCaps {
        tools: a.tools || b.tools,
        vision: a.vision || b.vision,
        streaming: a.streaming || b.streaming,
        json_mode: a.json_mode || b.json_mode,
    }
}
