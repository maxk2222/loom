-- Per-key queue priority override.
--   NULL  — inherit the owning project's priority_class (default, previous
--           behaviour: all keys of a project share its class).
--   0..N  — explicit class; effective request class is
--           COALESCE(api_keys.priority_class, projects.priority_class)
--           clamped to [0, scheduler.priority_classes-1]. Class 0 is the
--           highest priority queue (popped first, strict ordering).
ALTER TABLE api_keys
    ADD COLUMN IF NOT EXISTS priority_class INTEGER;
