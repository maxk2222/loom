-- 0001_init.sql — core schema: users/sessions/settings, projects, api keys,
-- clients, deployments, virtual models, config versioning, usage/audit.

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TABLE users (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name  TEXT NOT NULL DEFAULT '',
    role          TEXT NOT NULL DEFAULT 'viewer' CHECK (role IN ('admin','operator','viewer')),
    disabled      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    ip          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX sessions_user_idx ON sessions(user_id);

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE projects (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug           TEXT NOT NULL UNIQUE,
    name           TEXT NOT NULL,
    priority_class INTEGER NOT NULL DEFAULT 1,
    weight         INTEGER NOT NULL DEFAULT 1,
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE project_members (
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'member',
    PRIMARY KEY (project_id, user_id)
);

CREATE TABLE clients (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name             TEXT NOT NULL UNIQUE,
    credential_hash  TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending','active','suspended')),
    last_seen_at     TIMESTAMPTZ,
    version          TEXT NOT NULL DEFAULT '',
    platform         TEXT NOT NULL DEFAULT '',
    labels           JSONB NOT NULL DEFAULT '{}',
    desired_config   JSONB NOT NULL DEFAULT '{}',
    config_version   BIGINT NOT NULL DEFAULT 0,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE enroll_tokens (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash  TEXT NOT NULL UNIQUE,
    label       TEXT NOT NULL DEFAULT '',
    max_uses    INTEGER NOT NULL DEFAULT 1,
    used        INTEGER NOT NULL DEFAULT 0,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_by  UUID REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE deployments (
    id              UUID PRIMARY KEY,
    client_id       UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    backend_type    TEXT NOT NULL,
    endpoint_url    TEXT NOT NULL DEFAULT '',  -- metadata only: server never connects
    model_name      TEXT NOT NULL,
    capabilities    JSONB NOT NULL DEFAULT '{}',
    max_concurrent  INTEGER NOT NULL DEFAULT 4,
    local_reserve   INTEGER NOT NULL DEFAULT 1,
    tier2_max       INTEGER NOT NULL DEFAULT 2,
    sharing_mode    TEXT NOT NULL DEFAULT 'private'
                    CHECK (sharing_mode IN ('private','opportunistic','shared')),
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    reported        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deployments_client_idx ON deployments(client_id);

CREATE TABLE virtual_models (
    id                        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    api_name                  TEXT NOT NULL UNIQUE,
    description               TEXT NOT NULL DEFAULT '',
    api_type                  TEXT NOT NULL DEFAULT 'chat'
                              CHECK (api_type IN ('chat','completions','embeddings','multi')),
    routing_strategy          TEXT NOT NULL DEFAULT 'priority'
                              CHECK (routing_strategy IN
                                ('priority','least_in_flight','weighted','round_robin','manual')),
    required_capabilities     JSONB NOT NULL DEFAULT '{}',
    allow_heterogeneous       BOOLEAN NOT NULL DEFAULT FALSE,
    fallback_virtual_model_id UUID REFERENCES virtual_models(id) ON DELETE SET NULL,
    enabled                   BOOLEAN NOT NULL DEFAULT TRUE,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE virtual_model_members (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    virtual_model_id UUID NOT NULL REFERENCES virtual_models(id) ON DELETE CASCADE,
    deployment_id    UUID NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    priority         INTEGER NOT NULL DEFAULT 100,
    weight           INTEGER NOT NULL DEFAULT 1,
    manual_pinned    BOOLEAN NOT NULL DEFAULT FALSE,
    enabled          BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE (virtual_model_id, deployment_id)
);
CREATE INDEX vmm_vm_idx ON virtual_model_members(virtual_model_id);
CREATE INDEX vmm_dep_idx ON virtual_model_members(deployment_id);

CREATE TABLE project_models (
    project_id       UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    virtual_model_id UUID NOT NULL REFERENCES virtual_models(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, virtual_model_id)
);

CREATE TABLE api_keys (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL DEFAULT '',
    key_hash        TEXT NOT NULL UNIQUE,
    prefix          TEXT NOT NULL,
    scopes          JSONB NOT NULL DEFAULT '["chat","completions","embeddings","models"]',
    rpm_limit       INTEGER NOT NULL DEFAULT 60,
    tpm_limit       INTEGER NOT NULL DEFAULT 100000,
    max_concurrent  INTEGER NOT NULL DEFAULT 4,
    allowed_models  JSONB NOT NULL DEFAULT '[]',  -- [] = project grants, ["*"] = all
    expires_at      TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at    TIMESTAMPTZ
);
CREATE INDEX api_keys_project_idx ON api_keys(project_id);

CREATE TABLE routing_policies (
    key   TEXT PRIMARY KEY,
    value JSONB NOT NULL
);

CREATE TABLE config_versions (
    id         BIGSERIAL PRIMARY KEY,
    entity     TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    change     JSONB NOT NULL,
    actor      TEXT NOT NULL DEFAULT 'system',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX config_versions_entity_idx ON config_versions(entity, entity_id);

CREATE TABLE outbox_events (
    id            BIGSERIAL PRIMARY KEY,
    target        TEXT NOT NULL,            -- 'client:<uuid>' or 'broadcast'
    event_type    TEXT NOT NULL,
    payload       JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    dispatched_at TIMESTAMPTZ
);
CREATE INDEX outbox_pending_idx ON outbox_events(id) WHERE dispatched_at IS NULL;

CREATE TABLE requests_history (
    id               UUID PRIMARY KEY,
    api_key_id       UUID,
    project_id       UUID,
    virtual_model_id UUID,
    deployment_id    UUID,
    client_id        UUID,
    status           TEXT NOT NULL,
    queue_wait_ms    BIGINT NOT NULL DEFAULT 0,
    ttft_ms          BIGINT NOT NULL DEFAULT 0,
    duration_ms      BIGINT NOT NULL DEFAULT 0,
    prompt_tokens    BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    error_code       TEXT NOT NULL DEFAULT '',
    routing_trace    JSONB NOT NULL DEFAULT '{}',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX requests_history_created_idx ON requests_history(created_at);
CREATE INDEX requests_history_project_idx ON requests_history(project_id);

CREATE TABLE usage_counters (
    bucket            TIMESTAMPTZ NOT NULL,  -- truncated to hour
    project_id        UUID NOT NULL,
    api_key_id        UUID NOT NULL,
    virtual_model_id  UUID NOT NULL,
    requests          BIGINT NOT NULL DEFAULT 0,
    prompt_tokens     BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, project_id, api_key_id, virtual_model_id)
);

CREATE TABLE audit_logs (
    id         BIGSERIAL PRIMARY KEY,
    actor      TEXT NOT NULL,  -- 'user:<uuid>' | 'apikey:<uuid>' | 'system'
    action     TEXT NOT NULL,
    entity     TEXT NOT NULL,
    entity_id  TEXT NOT NULL DEFAULT '',
    diff       JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_logs_created_idx ON audit_logs(created_at);
