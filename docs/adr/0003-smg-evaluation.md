# ADR-0003: SMG (Shepherd Model Gateway) evaluation — not adopted

## Status

Accepted (Phase 0, time-boxed evaluation)

## Context

The architecture document asked to evaluate SMG (smg-project/smg) as a
candidate core before committing to a custom implementation.

## Evaluation result

SMG is a real Rust inference gateway with router + worker management, but its
deployment topology is **server-initiated**: the gateway connects *to* worker
endpoints. LOOM requires the opposite — workstation clients initiate the
channel toward the server (NAT/firewall traversal on lab machines).

Additional mismatches:

- Local-first admission control (local workloads always win) is not part of
  SMG's scheduling model.
- The client-side capability/sharing contract (tier-2 offered capacity that
  shrinks under local pressure) has no equivalent.
- Adopting it would mean reimplementing the control plane anyway.

## Decision

Do **not** adopt SMG. Implement the gateway natively (ADR-0001/0002). The
evaluation is time-boxed closed; re-evaluation only if SMG gains
client-initiated worker channels.

## Consequences

- Full control over protocol, admission semantics and fencing.
- More implementation work, absorbed by the phased plan.
