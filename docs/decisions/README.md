# Architecture decision records

Why MES-Intelligence is built the way it is. Each record states the context, the decision, the
alternatives considered and the consequences. The decisions that the MES shares with the ERP (the
Unified Namespace, idempotency) are recorded in
[ERP-Intelligence's decisions](https://github.com/mokwenaanthony-dev/ERP-Intelligence/tree/main/docs/decisions);
these records cover the MES's own choices.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-store-and-forward-with-event-ids.md) | Store-and-forward with the event ID as the idempotency key | Accepted |
| [0002](0002-the-erp-owns-the-plan-the-mes-owns-the-execution.md) | The ERP owns the plan, the MES owns the execution | Accepted |
| [0003](0003-sqlite-and-plain-sql.md) | A local SQLite database, written with plain SQL | Accepted |
| [0004](0004-simulator-before-the-edge-agent.md) | A machine simulator first, the Rust edge agent next | Accepted |
