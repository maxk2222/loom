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
    /// Effective KV budget in tokens: admin override falling back to the
    /// client-reported adapter capacity. None = slot-only admission.
    pub kv_budget: Option<i64>,
    /// How concurrency is bounded: "auto" (token gate when budget exists),
    /// "slots" (concurrency-only, budget ignored), "tokens" (token gate
    /// required — no budget = admits nothing).
    pub admission_mode: String,
}

impl Member {
    /// Routable only when both sides are enabled.
    pub fn enabled(&self) -> bool {
        self.member_enabled && self.dep_enabled
    }
}

impl VirtualModel {
    /// Whether this vm may serve a request for `api`. A typed vm only
    /// serves its declared api_type; `multi` is the wildcard for omni
    /// models bundling members of different endpoint families. `direct`
    /// vms are implicit per-deployment aliases whose stored api_type is a
    /// placeholder ('chat') — the single member's own capabilities remain
    /// the authority there.
    pub fn serves_api(&self, api: ApiType) -> bool {
        self.kind == "direct" || self.api_type == ApiType::Multi || self.api_type == api
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
            kv_budget: Option<i64>,
            admission_mode: String,
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
                    m.enabled AS member_enabled, d.enabled AS dep_enabled,
                    COALESCE(d.kv_budget_tokens, d.reported_kv_budget_tokens) AS kv_budget,
                    COALESCE(d.admission_mode, 'auto') AS admission_mode
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
                    kv_budget: m.kv_budget,
                    admission_mode: m.admission_mode,
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
                // for apis the local client can authoritatively admit
                // (chat/completions). Embeddings and media apis require an
                // explicit probed/declared flag: an unprobed deployment must
                // never absorb image/audio requests, and in an omni model it
                // must not steal embedding traffic either — embedding calls
                // cannot fall back mid-request, they only fail upstream.
                let unknown = m.capabilities == Capabilities::default();
                (unknown && api.admits_unprobed())
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

    /// Re-read the effective KV budget (override ?? reported) from Postgres
    /// and patch it onto every member pointing at this deployment — without
    /// this a newly-reported adapter budget stays invisible to lease
    /// admission until the next full snapshot reload.
    pub async fn patch_kv_budget(&self, deployment_id: &str) {
        let effective: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(kv_budget_tokens, reported_kv_budget_tokens) \
             FROM deployments WHERE id = $1::uuid",
        )
        .bind(deployment_id)
        .fetch_optional(&self.db)
        .await
        .unwrap_or_default()
        .flatten();
        let mut snap = self.snapshot.write().await;
        for vm in snap.values_mut() {
            for m in &mut vm.members {
                if m.deployment_id == deployment_id {
                    m.kv_budget = effective;
                }
            }
        }
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
        video: a.video || b.video,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `candidates()` never queries Postgres — a lazy pool suffices.
    fn registry() -> ModelRegistry {
        let pool =
            PgPool::connect_lazy("postgres://loom:loom@127.0.0.1:1/loom_test").expect("lazy pool");
        ModelRegistry::new(pool)
    }

    fn vm(api_type: ApiType, allow_hetero: bool, members: Vec<Member>) -> VirtualModel {
        VirtualModel {
            id: "vm1".into(),
            api_name: "omni".into(),
            api_type,
            routing_strategy: "priority".into(),
            required_caps: RequiredCaps::default(),
            allow_heterogeneous: allow_hetero,
            fallback_virtual_model_id: None,
            members,
            kind: "vm".into(),
            router_config: serde_json::json!({}),
        }
    }

    fn member(id: &str, model: &str, caps: Capabilities) -> Member {
        Member {
            deployment_id: id.into(),
            client_id: "c1".into(),
            model_name: model.into(),
            capabilities: caps,
            priority: 10,
            weight: 1,
            manual_pinned: false,
            member_enabled: true,
            dep_enabled: true,
            kv_budget: None,
            admission_mode: "slots".into(),
        }
    }

    fn caps_chat() -> Capabilities {
        Capabilities {
            chat: true,
            streaming: true,
            ..Default::default()
        }
    }

    fn caps_embed() -> Capabilities {
        Capabilities {
            embeddings: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn omni_vm_routes_each_api_to_matching_member() {
        let reg = registry();
        let v = vm(
            ApiType::Multi,
            true,
            vec![
                member("dep-chat", "qwen3", caps_chat()),
                member("dep-embed", "nomic-embed", caps_embed()),
                member(
                    "dep-img",
                    "sdxl",
                    Capabilities {
                        images: true,
                        ..Default::default()
                    },
                ),
            ],
        );
        let req = RequiredCaps::default();

        let chat = reg.candidates(&v, ApiType::Chat, &req).await;
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].deployment_id, "dep-chat");

        let emb = reg.candidates(&v, ApiType::Embeddings, &req).await;
        assert_eq!(emb.len(), 1);
        assert_eq!(emb[0].deployment_id, "dep-embed");

        let img = reg.candidates(&v, ApiType::Images, &req).await;
        assert_eq!(img.len(), 1);
        assert_eq!(img[0].deployment_id, "dep-img");

        // A family no member serves yields nothing — the scheduler then
        // reports NoCandidates rather than mis-routing.
        assert!(reg
            .candidates(&v, ApiType::AudioSpeech, &req)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn unprobed_member_admits_text_but_not_embeddings_or_media() {
        let reg = registry();
        let v = vm(
            ApiType::Multi,
            true,
            vec![member("dep-new", "anything", Capabilities::default())],
        );
        let req = RequiredCaps::default();
        assert_eq!(reg.candidates(&v, ApiType::Chat, &req).await.len(), 1);
        assert_eq!(
            reg.candidates(&v, ApiType::Completions, &req).await.len(),
            1
        );
        assert!(reg
            .candidates(&v, ApiType::Embeddings, &req)
            .await
            .is_empty());
        assert!(reg.candidates(&v, ApiType::Images, &req).await.is_empty());
        assert!(reg
            .candidates(&v, ApiType::AudioTranscription, &req)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn heterogeneity_applies_within_filtered_family() {
        let reg = registry();
        // allow_heterogeneous=false: a chat + an embed member with different
        // model names must not collide — the name check runs on the
        // api-filtered set only.
        let v = vm(
            ApiType::Multi,
            false,
            vec![
                member("dep-chat", "qwen3", caps_chat()),
                member("dep-embed", "nomic-embed", caps_embed()),
            ],
        );
        let req = RequiredCaps::default();
        assert_eq!(reg.candidates(&v, ApiType::Chat, &req).await.len(), 1);
        assert_eq!(reg.candidates(&v, ApiType::Embeddings, &req).await.len(), 1);

        // But two chat members with different model names collapse to the
        // canonical (lowest-priority-value) family.
        let mut a = member("dep-a", "model-a", caps_chat());
        a.priority = 5;
        let b = member("dep-b", "model-b", caps_chat());
        let v2 = vm(ApiType::Chat, false, vec![a, b]);
        let out = reg.candidates(&v2, ApiType::Chat, &req).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].deployment_id, "dep-a");
    }

    #[tokio::test]
    async fn required_caps_still_filter_within_family() {
        let reg = registry();
        let v = vm(
            ApiType::Multi,
            true,
            vec![
                member("dep-plain", "qwen3", caps_chat()),
                member(
                    "dep-tools",
                    "qwen3-tools",
                    Capabilities {
                        chat: true,
                        tools: true,
                        ..Default::default()
                    },
                ),
            ],
        );
        let req = RequiredCaps {
            tools: true,
            ..Default::default()
        };
        let out = reg.candidates(&v, ApiType::Chat, &req).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].deployment_id, "dep-tools");
    }

    #[test]
    fn serves_api_contract() {
        let mut v = vm(ApiType::Multi, true, vec![]);
        for api in [
            ApiType::Chat,
            ApiType::Embeddings,
            ApiType::Images,
            ApiType::Video,
        ] {
            assert!(v.serves_api(api), "multi serves {api}");
        }
        v.api_type = ApiType::Chat;
        assert!(v.serves_api(ApiType::Chat));
        assert!(!v.serves_api(ApiType::Embeddings));
        assert!(!v.serves_api(ApiType::Images));
        v.api_type = ApiType::Embeddings;
        assert!(!v.serves_api(ApiType::Chat));
        assert!(v.serves_api(ApiType::Embeddings));

        // 'direct' vms are implicit per-deployment aliases — their stored
        // api_type is a 'chat' placeholder and must not reject any api.
        v.kind = "direct".into();
        v.api_type = ApiType::Chat;
        assert!(v.serves_api(ApiType::Embeddings));
        assert!(v.serves_api(ApiType::Images));
    }
}
