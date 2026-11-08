export interface Branding {
  app_name: string;
  tagline: string;
  logo_path: string;
  accent_color: string;
}

export interface UserCtx {
  id: string;
  role: "admin" | "operator" | "viewer";
  email: string;
}

export interface Overview {
  clients_total: number;
  clients_online: number;
  virtual_models: number;
  queued_5m: number;
  active_5m: number;
  requests_24h: number;
}

export interface ClientRow {
  id: string;
  name: string;
  status: string;
  online: boolean;
  last_seen_at: string | null;
  version: string;
  platform: string;
  labels: Record<string, unknown>;
  config_version: number;
  stats?: ClientStats | null;
}

export interface ClientStats {
  cpu_pct: number | null;
  ram_pct: number | null;
  ram_used_bytes: number | null;
  ram_total_bytes: number | null;
  gpu?: {
    name: string;
    util_pct: number | null;
    mem_used_bytes: number | null;
    mem_total_bytes: number | null;
  } | null;
  at: number | null;
}

export interface DeploymentRow {
  id: string;
  client_id: string;
  client_name?: string;
  name: string;
  backend_type: string;
  model_name: string;
  enabled: boolean;
  capabilities?: Record<string, unknown> | null;
  runtime?: { state: string; tier2_free: number; tier2_total: number; pressure_gated?: boolean } | null;
}

export interface VirtualModel {
  id: string;
  api_name: string;
  api_type: string;
  routing_strategy: string;
  description: string;
  enabled: boolean;
  allow_heterogeneous: boolean;
  fallback_virtual_model_id?: string | null;
  required_capabilities?: Record<string, unknown>;
  kind?: string;
}

export interface Member {
  id?: string;
  deployment_id: string;
  priority: number;
  weight: number;
  manual_pinned: boolean;
  enabled: boolean;
}

export interface ApiUser {
  id: string;
  slug: string;
  name: string;
  priority_class: number;
  weight: number;
  enabled: boolean;
}

export interface ApiKey {
  id: string;
  project_id: string;
  name: string;
  prefix: string;
  rpm_limit: number;
  tpm_limit: number;
  max_concurrent: number;
  active: boolean;
}

export interface UserRow {
  id: string;
  email: string;
  display_name: string;
  role: string;
  disabled: boolean;
}

export interface HistoryRow {
  id: string;
  status: string;
  queue_wait_ms: number;
  ttft_ms: number;
  duration_ms: number;
  prompt_tokens: number;
  completion_tokens: number;
  error_code: string;
  created_at: string;
}

export interface UsageStat {
  project_id: string;
  requests: number;
  prompt_tokens: number;
  completion_tokens: number;
}

export interface QueueClass {
  class: number;
  depth: number;
}

export interface SystemInfo {
  version: string;
  postgres: boolean;
  redis: boolean;
}

// client variant types
export interface ClientStatus {
  name: string;
  version: string;
  uptime_secs: number;
  server_connected: boolean;
  server_url: string;
  sharing_enabled: boolean;
  capacities: CapacityInfo[];
  system?: SystemStats;
}

export interface SystemStats {
  cpu_pct: number;
  ram_pct: number;
  ram_used_bytes: number;
  ram_total_bytes: number;
  pressure: boolean;
  cooldown: boolean;
  cpu_limit_pct: number;
  ram_limit_pct: number;
  gpu_vram_limit_pct?: number;
  enabled: boolean;
  gpu?: {
    name: string;
    util_pct: number | null;
    mem_used_bytes: number | null;
    mem_total_bytes: number | null;
  } | null;
}

export interface ClientNote {
  id: string;
  title: string;
  body: string;
  severity: string;
  persistent: boolean;
  created_at_unix: number;
  expires_at_unix: number;
  dismissed: boolean;
}

export interface CapacityInfo {
  deployment_id: string;
  tier2_total: number;
  tier2_free: number;
  inflight_local: number;
  inflight_tier2: number;
  sharing_mode: string;
  accepting: boolean;
  pressure_gated?: boolean;
}

export interface LocalDeployment {
  deployment: {
    id: string;
    name: string;
    model_name: string;
    max_concurrent: number;
    local_reserve: number;
    tier2_max: number;
    sharing_mode: string;
    enabled: boolean;
  };
  backend_id: string;
  backend_type: string;
  capabilities: Record<string, unknown> | null;
  health: string | null;
}

export interface LocalRequest {
  request_id: string;
  class: string;
  deployment_id: string;
  api: string;
  started_unix_ms: number;
}
