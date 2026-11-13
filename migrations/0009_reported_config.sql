-- deployments.{tier2_max,local_reserve,sharing_mode,max_concurrent} are no
-- longer unused defaults: capacity reports now persist the client's
-- last-reported config. Redis dep:{id} hashes remain the live authority;
-- clients.desired_config remains the admin-intent channel. The columns
-- read as "what the client last reported" — tier2_max = mesh share
-- offered (0 while private/disabled), max_concurrent/local_reserve are
-- untouched when older clients do not send them.
COMMENT ON COLUMN deployments.tier2_max IS
    'last client-reported tier-2 share capacity (0 while private/disabled)';
COMMENT ON COLUMN deployments.local_reserve IS
    'last client-reported local-reserve slots (kept when older clients omit it)';
COMMENT ON COLUMN deployments.sharing_mode IS
    'last client-reported sharing mode: private|opportunistic|shared';
COMMENT ON COLUMN deployments.max_concurrent IS
    'last client-reported total admission slots (kept when older clients omit it)';
