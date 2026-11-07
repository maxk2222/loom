-- Auto-router: virtual models can act as LLM-driven routers that classify a
-- request and forward it to one of their configured member models.
-- kind: 'vm'     — normal virtual model (default)
--       'router' — LLM classifier; members come from router_config
--       'direct' — implicit single-deployment model (router-internal)
ALTER TABLE virtual_models
    ADD COLUMN kind          TEXT  NOT NULL DEFAULT 'vm'
                             CHECK (kind IN ('vm','router','direct')),
    ADD COLUMN router_config JSONB NOT NULL DEFAULT '{}';

-- router_config shape:
-- {
--   "router_model":  "<api_name of the vm used for classification>",
--   "system_prompt": "<custom prompt template; {{models}} and {{request}} are
--                     substituted>",
--   "fallback_model":"<api_name used when classification fails>",
--   "max_user_chars": 2000,
--   "members": [ {"ref": "vm:<api_name>" | "dep:<deployment-uuid>",
--                 "name": "<choice name the router replies with>",
--                 "description": "<what this model is good at>"} ]
-- }

CREATE INDEX vm_kind_idx ON virtual_models(kind) WHERE kind <> 'vm';
