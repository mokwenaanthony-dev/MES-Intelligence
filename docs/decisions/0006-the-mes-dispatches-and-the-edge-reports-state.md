# 0006. The MES dispatches; the edge agent reports retained state

- **Status:** Accepted, 2026-11-12 (MES 0.2.0)

## Context

With a real machine behind an edge agent, someone must decide which operation the machine runs
next, and someone must tell the ERP what was done. ERP contract v1 says the MES publishes
`operation-performance`. The flow must survive any component restarting at any moment without
running a job twice or posting it twice (ERP ADR 0010).

## Decision

- **The MES dispatches** (ISA-95 Level 3 owns dispatching). For each `--dispatch` cell it keeps one
  job on `<cell>/schedule/current-job`, retained: the running request, else the oldest queued one,
  else nothing. It publishes only when that changes.
- **The edge agent executes and reports state, not events.** It reconciles "the job the MES wants"
  with "what the PLC reports" in a loop, and publishes retained `job-progress/v1` and
  `equipment-state/v1`. State needs no event IDs and no store-and-forward: after any outage the
  latest state is simply published again.
- **The MES reports to the ERP.** On COMPLETE it sends one `operation-performance/v1` event with the
  counted quantities and the measured hours for every planned activity, then dispatches the next
  job. The contract with the ERP does not change.
- **Deterministic event ID** for dispatched jobs: `<cell>-<order>-<operation>`. If an event with that
  ID is already stored, the MES resends the stored bytes instead of building a new message, so a
  crash between "sent" and "marked DONE" cannot produce a second, different event.
- **The agent remembers its job** in a small state file (written to a temporary file, then renamed:
  atomic), so after a restart it resumes a running job and reports a finished one instead of
  starting either again.
- **Labour hours = machine hours** for a dispatched cell: the PLC measures machine time, and the
  cell's operator attends it for that time.

## Alternatives considered

- **The edge agent picks work from the ERP's operation requests itself:** no MES in the loop, so no
  dispatching rules, and the agent would have to publish the ERP's events and keep them until
  answered.
- **The edge agent publishes operation-performance directly:** changes who publishes in ERP contract
  v1, and the agent would need the store-and-forward outbox the MES already has.
- **Events from the agent (job-started, job-finished) instead of state:** each needs an ID, storage
  until acknowledged, and ordering; retained state needs none of these.

## Consequences

- Any component can restart at any time; the worst case is a message published again, which
  changes nothing.
- Labour measured separately (operator log-on, a second counter) would be a new optional field in
  `job-progress/v1`, not a new version.
- A change of a running request's quantity in the ERP does not reach the machine until the next
  job (the job ID is unchanged); cancelling the order does (the request disappears and the MES
  moves on).
