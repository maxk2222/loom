//! Atomic Redis Lua scripts. Keys/args are always fully-qualified (prefix
//! applied by caller).

/// queue_enqueue: KEYS=[queue:{prio}, req:{id}, queue:count, req:watchdog]
/// ARGV=[request_id, enqueue_ts, max_queue, project_id, vm_id, prio, deadline_ts]
/// -> "OK" | "QUEUE_FULL"
pub const QUEUE_ENQUEUE: &str = r#"
local qkey = KEYS[1]
local rkey = KEYS[2]
local rqid = ARGV[1]
local ets  = tonumber(ARGV[2])
local maxq = tonumber(ARGV[3])
-- global queued-request counter across all class queues
local cnt = tonumber(redis.call('GET', KEYS[3]) or '0')
if cnt >= maxq then return 'QUEUE_FULL' end
redis.call('INCR', KEYS[3])
redis.call('ZADD', qkey, ets, rqid)
redis.call('HSET', rkey,
  'state', 'queued',
  'project_id', ARGV[4],
  'vm_id', ARGV[5],
  'prio', ARGV[6],
  'enqueued_at', ARGV[2],
  'deadline', ARGV[7])
-- attempts must survive requeue (HSETNX) or max_attempts can never fire
redis.call('HSETNX', rkey, 'attempts', '0')
redis.call('EXPIRE', rkey, 7200)
redis.call('ZADD', KEYS[4], tonumber(ARGV[7]), rqid)
return 'OK'
"#;

/// queue_pop: KEYS=[queue:{p0}, queue:{p1}, ...] (highest prio first)
/// ARGV=[now_ms]
/// -> request_id | nil ; sets req state queued->leased via separate call
/// NOTE: pops only, state transition by caller for clarity.
pub const QUEUE_POP: &str = r#"
for i=1,#KEYS do
  local v = redis.call('ZPOPMIN', KEYS[i], 1)
  if v[1] then return v[1] end
end
return nil
"#;

/// queue_remove: KEYS=[queue:{prio}, req:{id}, watch_key]
/// ARGV=[request_id] -> "OK" | "NOT_FOUND"
pub const QUEUE_REMOVE: &str = r#"
local n = redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
if n > 0 then
  redis.call('HSET', KEYS[2], 'state', 'cancelled')
  redis.call('DECR', KEYS[4])
  return 'OK'
end
return 'NOT_FOUND'
"#;

/// lease_acquire: KEYS=[dep:{id}, dep:{id}:leases, lease:{lid}, cap:fencing]
/// ARGV=[request_id, client_id, dep_id, ttl_ms, now_ms, fencing_in, fp, kv_budget]
/// -> {fencing} | "NO_CAPACITY" | "KV_FULL" | "UNAVAILABLE"
/// Idempotent per request_id (same lease returned).
/// `kv_used` always tracks the sum of live lease footprints; the budget gate
/// applies only when kv_budget > 0 (unset = pure slot counting).
pub const LEASE_ACQUIRE: &str = r#"
local dep = KEYS[1]
local leases = KEYS[2]
local lkey = KEYS[3]
local state = redis.call('HGET', dep, 'state')
if state ~= 'available' then return 'UNAVAILABLE' end
local existing = redis.call('HGET', lkey, 'request_id')
if existing == ARGV[1] then
  return { tonumber(redis.call('HGET', lkey, 'fencing')) }
end
local free = tonumber(redis.call('HGET', dep, 'tier2_free') or '0')
if free <= 0 then return 'NO_CAPACITY' end
local fp = tonumber(ARGV[7]) or 0
local budget = tonumber(ARGV[8]) or 0
if budget > 0 then
  local used = tonumber(redis.call('HGET', dep, 'kv_used') or '0')
  if used + fp > budget then return 'KV_FULL' end
end
redis.call('HINCRBY', dep, 'tier2_free', -1)
redis.call('HINCRBY', dep, 'kv_used', fp)
local fencing = redis.call('INCR', KEYS[4])
local exp = tonumber(ARGV[5]) + tonumber(ARGV[4])
redis.call('HSET', lkey,
  'request_id', ARGV[1],
  'client_id', ARGV[2],
  'dep_id', ARGV[3],
  'fencing', fencing,
  'state', 'reserved',
  'created', ARGV[5],
  'expires', exp,
  'fp', fp)
redis.call('PEXPIRE', lkey, tonumber(ARGV[4]) * 3)
redis.call('ZADD', leases, exp, KEYS[3])
return { fencing }
"#;

/// lease_confirm: KEYS=[lease:{lid}, dep:{id}]
/// ARGV=[fencing] -> "OK" | "STALE" | "NOT_FOUND"
pub const LEASE_CONFIRM: &str = r#"
local f = redis.call('HGET', KEYS[1], 'fencing')
if not f then return 'NOT_FOUND' end
if tonumber(f) ~= tonumber(ARGV[1]) then return 'STALE' end
redis.call('HSET', KEYS[1], 'state', 'confirmed')
return 'OK'
"#;

/// lease_activate: reserved|confirmed -> active (request streaming)
/// KEYS=[lease:{lid}] ARGV=[fencing, now_ms] -> "OK"|"STALE"|"NOT_FOUND"
pub const LEASE_ACTIVATE: &str = r#"
local f = redis.call('HGET', KEYS[1], 'fencing')
if not f then return 'NOT_FOUND' end
if tonumber(f) ~= tonumber(ARGV[1]) then return 'STALE' end
redis.call('HSET', KEYS[1], 'state', 'active', 'activated_at', ARGV[2])
return 'OK'
"#;

/// active_lease_sweep: fail active leases whose stream has been silent longer
/// than the idle deadline — `activated_at` is refreshed by the server on each
/// inbound data chunk, so this only kills genuinely stalled requests. A
/// client that accepts work and then hangs forever (backend stuck, stream
/// never yields) would otherwise burn the tier-2 slot indefinitely.
/// KEYS=[dep:{id}:leases, dep:{id}] ARGV=[now_ms, deadline_ms]
/// -> {request_ids freed}  (frees the slot atomically inside the script)
pub const ACTIVE_LEASE_SWEEP: &str = r#"
local lset = KEYS[1]
local dep = KEYS[2]
local now = tonumber(ARGV[1])
local deadline = tonumber(ARGV[2])
local out = {}
local leases = redis.call('ZRANGE', lset, 0, -1)
for _,lkey in ipairs(leases) do
  local st = redis.call('HGET', lkey, 'state')
  local at = tonumber(redis.call('HGET', lkey, 'activated_at') or '0')
  if st == 'active' and at > 0 and now - at > deadline then
    local rq = redis.call('HGET', lkey, 'request_id')
    local fp = tonumber(redis.call('HGET', lkey, 'fp') or '0')
    table.insert(out, rq or lkey)
    redis.call('DEL', lkey)
    redis.call('ZREM', lset, lkey)
    redis.call('HINCRBY', dep, 'tier2_free', 1)
    if fp > 0 then
      local u = redis.call('HINCRBY', dep, 'kv_used', -fp)
      if u < 0 then redis.call('HSET', dep, 'kv_used', 0) end
    end
  end
end
return out
"#;

/// lease_release: KEYS=[lease:{lid}, dep:{id}, dep:{id}:leases]
/// ARGV=[fencing, request_id] -> "OK" | "STALE" | "NOT_FOUND"
/// Idempotent: only frees the slot if lease still held. Gives back the
/// lease's token footprint alongside the slot.
pub const LEASE_RELEASE: &str = r#"
local f = redis.call('HGET', KEYS[1], 'fencing')
if not f then return 'NOT_FOUND' end
if tonumber(f) ~= tonumber(ARGV[1]) then return 'STALE' end
local fp = tonumber(redis.call('HGET', KEYS[1], 'fp') or '0')
redis.call('DEL', KEYS[1])
redis.call('ZREM', KEYS[3], KEYS[1])
redis.call('HINCRBY', KEYS[2], 'tier2_free', 1)
if fp > 0 then
  local u = redis.call('HINCRBY', KEYS[2], 'kv_used', -fp)
  if u < 0 then redis.call('HSET', KEYS[2], 'kv_used', 0) end
end
return 'OK'
"#;

/// lease_fp_update: converge a lease's reserved footprint to the client's
/// reported cumulative token usage mid-flight. Monotonic — actuals only
/// ever grow a reservation (usage can lag but never shrink), so a stale or
/// lower report is ignored instead of opening the gate against reality.
/// KEYS=[lease:{lid}, dep:{id}] ARGV=[fencing, tokens_used]
/// -> "OK" | "STALE" | "NOT_FOUND"
pub const LEASE_FP_UPDATE: &str = r#"
local f = redis.call('HGET', KEYS[1], 'fencing')
if not f then return 'NOT_FOUND' end
if tonumber(f) ~= tonumber(ARGV[1]) then return 'STALE' end
local cur = tonumber(redis.call('HGET', KEYS[1], 'fp') or '0')
local actual = tonumber(ARGV[2]) or 0
if actual > cur then
  redis.call('HSET', KEYS[1], 'fp', actual)
  redis.call('HINCRBY', KEYS[2], 'kv_used', actual - cur)
end
return 'OK'
"#;

/// client_offline: KEYS=[client:{id}] + ARGV=[dep keys json array not needed]
/// Instead: caller passes all dep keys + lease lists explicitly.
/// KEYS=[dep1, dep1:leases, dep2, dep2:leases, ...]
/// -> {reserved_request_ids..., active_request_ids...} flattened as array
pub const CLIENT_OFFLINE: &str = r#"
local reserved = {}
local active = {}
for i=1,#KEYS,2 do
  local dep = KEYS[i]
  local lset = KEYS[i+1]
  redis.call('HSET', dep, 'state', 'offline', 'tier2_free', 0)
  local leases = redis.call('ZRANGE', lset, 0, -1)
  for _,lkey in ipairs(leases) do
    local st = redis.call('HGET', lkey, 'state')
    local rq = redis.call('HGET', lkey, 'request_id')
    local fp = tonumber(redis.call('HGET', lkey, 'fp') or '0')
    if rq then
      if st == 'reserved' then
        table.insert(reserved, rq)
      else
        table.insert(active, rq)
      end
      redis.call('DEL', lkey)
      redis.call('ZREM', lset, lkey)
      if fp > 0 then
        local u = redis.call('HINCRBY', dep, 'kv_used', -fp)
        if u < 0 then redis.call('HSET', dep, 'kv_used', 0) end
      end
    end
  end
end
return {reserved, active}
"#;

/// Sweep expired leases for a deployment: KEYS=[dep:{id}, dep:{id}:leases]
/// ARGV=[now_ms] -> {request_ids released}
pub const LEASE_SWEEP: &str = r#"
local dep = KEYS[1]
local lset = KEYS[2]
local now = tonumber(ARGV[1])
local expired = redis.call('ZRANGEBYSCORE', lset, 0, now)
local out = {}
for _,lkey in ipairs(expired) do
  local rq = redis.call('HGET', lkey, 'request_id')
  local st = redis.call('HGET', lkey, 'state')
  if rq then
    -- only free 'reserved' leases; confirmed/active die via watchdog not sweep
    if st == 'reserved' then
      local fp = tonumber(redis.call('HGET', lkey, 'fp') or '0')
      table.insert(out, rq)
      redis.call('DEL', lkey)
      redis.call('ZREM', lset, lkey)
      redis.call('HINCRBY', dep, 'tier2_free', 1)
      if fp > 0 then
        local u = redis.call('HINCRBY', dep, 'kv_used', -fp)
        if u < 0 then redis.call('HSET', dep, 'kv_used', 0) end
      end
    end
  end
end
return out
"#;

/// rl_check: sliding-window rate limit + concurrency.
/// KEYS=[rl:key:{id}:rpm, inflight:key:{id}]
/// ARGV=[now_ms, window_ms, rpm_limit, max_concurrent]
/// -> "OK" | "RPM" | "CONCURRENCY"
pub const RL_CHECK: &str = r#"
local now = tonumber(ARGV[1])
local win = tonumber(ARGV[2])
local rpm = tonumber(ARGV[3])
local conc = tonumber(ARGV[4])
local inflight = tonumber(redis.call('GET', KEYS[2]) or '0')
if conc > 0 and inflight >= conc then return 'CONCURRENCY' end
if rpm > 0 then
  redis.call('ZREMRANGEBYSCORE', KEYS[1], 0, now - win)
  local c = redis.call('ZCARD', KEYS[1])
  if c >= rpm then return 'RPM' end
end
local tpm = tonumber(ARGV[5]) or 0
if tpm > 0 then
  redis.call('ZREMRANGEBYSCORE', KEYS[3], 0, now - win)
  local members = redis.call('ZRANGE', KEYS[3], 0, -1)
  local sum = 0
  for _,m in ipairs(members) do
    local t = m:match(':(%d+)$')
    if t then sum = sum + tonumber(t) end
  end
  if sum >= tpm then return 'TPM' end
end
return 'OK'
"#;

/// tpm_charge: record a completed request's token spend in the sliding
/// window zset (member = "request_id:total_tokens").
/// KEYS=[rl:key:{id}:tpm] ARGV=[now_ms, member, window_ms]
pub const TPM_CHARGE: &str = r#"
redis.call('ZREMRANGEBYSCORE', KEYS[1], 0, tonumber(ARGV[1]) - tonumber(ARGV[3]))
redis.call('ZADD', KEYS[1], tonumber(ARGV[1]), ARGV[2])
redis.call('EXPIRE', KEYS[1], math.ceil(tonumber(ARGV[3]) / 1000) + 10)
return 'OK'
"#;

/// rl_commit: record the request in the window + increment inflight.
/// KEYS=[rl:key:{id}:rpm, inflight:key:{id}] ARGV=[now_ms, request_id, window_ms]
pub const RL_COMMIT: &str = r#"
redis.call('ZADD', KEYS[1], tonumber(ARGV[1]), ARGV[2])
redis.call('EXPIRE', KEYS[1], math.ceil(tonumber(ARGV[3]) / 1000) + 10)
redis.call('INCR', KEYS[2])
redis.call('EXPIRE', KEYS[2], 7200)
return 'OK'
"#;

/// inflight_dec: decrement inflight counter for key/project.
pub const INFLIGHT_DEC: &str = r#"
local v = redis.call('DECR', KEYS[1])
if v < 0 then redis.call('SET', KEYS[1], 0) return 0 end
return v
"#;
