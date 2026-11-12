# Edge contract (version 1): the MES and its edge agents

The [Unified Namespace contract](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/UNS_CONTRACT.md)
is the agreement between the ERP (ISA-95 Level 4) and the MES (Level 3). This document is the
agreement **inside the shop floor**, between the MES and the edge agents next to its machines
(Level 2). The messages share the namespace and the envelope, but the ERP never reads them, so they
can change without a change to the ERP contract.

> Status: **version 1, MES 0.2.0**. Implemented in `src/mes/core.py` (MES) and `edge/src/messages.rs`
> (agent); `tests/test_edge.py` builds the agent's messages exactly as the Rust code writes them.

## Who does what

| Level | Component | Decides | Reports |
|---|---|---|---|
| 4 | ERP | which operations to run, where, how many | (reads operation-performance) |
| 3 | MES | which job each machine runs **now** (dispatching) | operation-performance to the ERP |
| 2 | Edge agent | how to tell its PLC (Modbus registers) | cell state, job progress |
| 1 | PLC | how to run the machine | registers: state, counters, run time |

A cell is either **simulated** by the MES (`--simulate`, no machine) or **dispatched** to an edge
agent (`--dispatch`), never both.

## Topics

| Topic | Publisher | Retained | Schema |
|---|---|---|---|
| `<cell>/schedule/current-job` | MES | yes | `job/v1`; empty payload = no job |
| `<cell>/state/job` | edge agent | yes | `job-progress/v1` |
| `<cell>/state/status` | edge agent | yes | `equipment-state/v1` (ERP contract v1) |

Everything here is **retained state**: the latest message is the truth, publishing it again
changes nothing, and a component that (re)starts gets the current picture at once. No events, so
no event IDs and nothing to store and forward at Level 2. QoS 1.

## Messages

Envelope as in the ERP contract: `{schema, event_id (null), timestamp, source, data}`. Quantities and
hours are decimal strings.

### `job/v1` (MES → `<cell>/schedule/current-job`)

```json
"data": {
  "job_id": "mo00000001-0010", "order": "MO00000001", "operation_number": 10,
  "quantity": "100.000", "unit": "EA", "material": "SF-3001",
  "planned_activities": {"LAB": "10.000", "MACH": "10.500"}
}
```

`job_id` is the last level of the ERP's operation-request topic (`<order>-<operation>`, lower case),
so one operation is one job. The agent makes `quantity` good parts, rounded up to a whole part.

### `job-progress/v1` (agent → `<cell>/state/job`)

```json
"data": {
  "job_id": "mo00000001-0010", "order": "MO00000001", "operation_number": 10,
  "status": "COMPLETE", "good_quantity": "100", "scrap_quantity": "2", "run_hours": "10.750"
}
```

`status` is `RUNNING` (at most once a second, only when the counters changed) or `COMPLETE` (once,
when the PLC reaches the target). `run_hours` is the machine time the PLC measured, rounded half up
to three decimals.

### `equipment-state/v1` (agent → `<cell>/state/status`)

As in ERP contract v1. The agent maps the PLC: running → `RUNNING`, idle or complete → `IDLE`,
fault → `DOWN`, unreachable → `OFFLINE`. The agent's MQTT **last will** is an `OFFLINE` state, so the
broker reports the cell offline if the agent dies.

## The flow

1. The ERP releases an order; its operation request is retained at the cell (ERP contract).
2. The MES queues it and, if the cell has no job, publishes it as the cell's current job and marks
   the request RUNNING.
3. The agent sees a job it has not started, writes the target and the start command to the PLC in
   one Modbus request, and remembers the job in its state file.
4. The agent reports progress and state while the PLC counts; then COMPLETE.
5. The MES turns COMPLETE into one `operation-performance/v1` event for the ERP: yield and scrap as
   counted, every planned activity at the measured hours, `final: true`, event ID
   `<cell>-<job id>` (e.g. `press3-mo00000001-0010`). It marks the request DONE and publishes the
   next job, or clears the topic. The agent resets the PLC when its job is no longer wanted.

## Idempotency (ERP ADR 0010)

| Repeat | Effect |
|---|---|
| The same `job/v1` again (QoS 1 redelivery, retained copy after reconnecting) | The agent already has it: nothing |
| The agent restarts mid-job | State file + PLC still running the job: it carries on, counters kept |
| The agent restarts after COMPLETE | State file says COMPLETE: the saved totals are reported again; the job is never run twice |
| The same COMPLETE again | The request is already DONE: ignored |
| The MES dies after sending the event, before marking DONE | The event ID comes from the job, so the stored event is resent byte for byte: the ERP posts it once |
| An empty current-job right after reconnecting | The agent waits 5 s (or for the MES's message) before believing "no job", so a slow retained delivery never resets a running machine |

## Versioning

As in the ERP contract: adding an optional field is not a version change; removing, renaming or
changing the meaning of one is a new schema version (`job/v2`), published beside the old one until
every agent has moved.
