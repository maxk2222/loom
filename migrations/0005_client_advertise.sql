-- Client self-advertisement: local admin UI URL + detected interface
-- addresses, reported in Hello so the dashboard can link the UI directly.
ALTER TABLE clients
    ADD COLUMN IF NOT EXISTS ui_url          TEXT  NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS advertise_addrs JSONB NOT NULL DEFAULT '[]';
