# 0002. The ERP owns the plan, the MES owns the execution

- **Status:** Accepted, 2026-11-12 (MES 0.1.0)

## Context

Both systems know about cells and operations. If both could change the same data, they would
disagree, and nobody could tell which was right (ISA-95 levels 4 and 3).

## Decision

- The **ERP owns master data and the plan**: which cells exist (`equipment/v1`) and which operations
  of released orders should run where (`operation-request/v1`). Both are **retained** topics, so an
  MES that starts or reconnects gets the whole current picture at once, without asking.
- The MES keeps a **copy** of them and never changes their content. An empty retained message means
  the ERP removed the cell or completed or cancelled the order: the MES drops its copy.
- The **MES owns execution**: what a cell is doing now (`equipment-state/v1`, retained) and the work
  actually done (`operation-performance/v1`, events). How far a request has got (QUEUED, RUNNING,
  DONE) is the MES's own column, which an update of the request from the ERP does not overwrite.
- The topic paths and message formats are fixed by the **contract**, kept in ERP-Intelligence
  ([`docs/UNS_CONTRACT.md`](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/UNS_CONTRACT.md)).
  This release implements contract version 1 (`mes.CONTRACT_VERSION`). `tests/erp_messages.py` holds
  the contract's examples, so a change of format breaks a test here.

## Alternatives considered

- **The MES asks the ERP's REST API for work:** couples the shop floor to the ERP being up, and
  needs credentials on every edge device.
- **A shared database:** the tightest coupling of all; ruled out by ERP ADR 0009.

## Consequences

- The MES can run for a whole shift without the ERP: its queue is whatever was last retained.
- Messages with an unknown schema are logged and skipped, so a newer ERP can add messages without
  breaking an older MES.
