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
    /// 'vm' | 'router' | 'direct' (implicit single-deployment, hidden).
    pub kind: String,
    /// Router config (kind='router'): members, prompt, fallback, engine.
    pub router_config: serde_json::Value,
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
    /// virtual_model_members.enabled (the VM-side toggle).
    pub member_enabled: bool,
    /// deployments.enabled (the client-side toggle — changes arrive with
    /// inventory reports and are patched into the snapshot live).
    pub dep_enabled: bool,
}

impl Member {
    /// Routable only when both sides are enabled.
    pub fn enabled(&self) -> bool {
        self.member_enabled && self.dep_enabled
    }
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
            kind: String,
            router_config: serde_json::Value,
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
            member_enabled: bool,
            dep_enabled: bool,
        }

        let vms: Vec<VmRow> = sqlx::query_as(
            "SELECT id, api_name, api_type, routing_strategy, required_capabilities,
                    allow_heterogeneous, fallback_virtual_model_id, kind, router_config
             FROM virtual_models WHERE enabled = true",
        )
        .fetch_all(&self.db)
        .await?;

        let members: Vec<MRow> = sqlx::query_as(
            "SELECT m.virtual_model_id, m.deployment_id, d.client_id, d.model_name,
                    d.capabilities, m.priority, m.weight, m.manual_pinned,
                    m.enabled AS member_enabled, d.enabled AS dep_enabled
             FROM virtual_model_members m
             JOIN deployments d ON d.id = m.deployment_id
             ORDER BY m.priority",
        )
        .fetch_all(&self.db)
        .await?;

        let mut by_member: HashMap<String, Vec<Member>> = HashMap::new();
        for m in members {
            let caps: Capabilities = serde_json::from_value(m.capabilities).unwrap_or_default();
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
                    member_enabled: m.member_enabled,
                    dep_enabled: m.dep_enabled,
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
                kind: v.kind,
                router_config: v.router_config,
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
    /// `direct` models are router-internal and hidden from callers.
    pub async fn list(&self) -> Vec<String> {
        self.snapshot
            .read()
            .await
            .iter()
            .filter(|(_, vm)| vm.kind != "direct")
            .map(|(n, _)| n.clone())
            .collect()
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
                if !m.enabled() {
                    return false;
                }
                // Unprobed members (reported all-false / empty) are permissive
                // for text-family apis — the client's local admission is
                // authoritative. Media apis require an explicit flag: an
                // unprobed chat deployment must never absorb image/audio
                // requests.
                let unknown = m.capabilities == Capabilities::default();
                (unknown && api.is_text_family())
                    || (m.capabilities.supports_api(api) && m.capabilities.satisfies(&merged))
            })
            .cloned()
            .collect();
        // If VM disallows heterogeneity, members must share one model_name.
        // The canonical name comes from the lowest-priority-value member —
        // vm.members arrive in arbitrary DB order, so out.first() alone
        // would make the surviving family nondeterministic across reloads.
        if !vm.allow_heterogeneous && out.len() > 1 {
            if let Some(first) = out
                .iter()
                .min_by_key(|m| m.priority)
                .map(|m| m.model_name.clone())
            {
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

    /// Patch live deployment attributes (capabilities, enabled flag) into
    /// the snapshot wherever the deployment appears as a member — without
    /// this, inventory reports only reach Postgres/Redis while routing
    /// decisions keep filtering on the stale snapshot until the next
    /// admin-triggered reload.
    pub async fn patch_deployment(
        &self,
        deployment_id: &str,
        caps: Option<Capabilities>,
        dep_enabled: Option<bool>,
    ) {
        let mut snap = self.snapshot.write().await;
        for vm in snap.values_mut() {
            for m in &mut vm.members {
                if m.deployment_id == deployment_id {
                    if let Some(c) = &caps {
                        m.capabilities = c.clone();
                    }
                    if let Some(e) = dep_enabled {
                        m.dep_enabled = e;
                    }
                }
            }
        }
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
