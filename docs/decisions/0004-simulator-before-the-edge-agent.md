# 0004. A machine simulator first, the Rust edge agent next

- **Status:** Accepted, 2026-11-12 (MES 0.1.0)

## Context

ERP ADR 0009 plans a Rust edge agent per machine, reading a PLC. Building it first would delay the
end-to-end flow (ERP order to shop floor and back to the ledger) behind PLC protocols and a second
language.

## Decision

- MES 0.1.0 ships a **`MachineSimulator`** inside the Python MES: for each queued operation it
  publishes RUNNING, "works" for the planned time scaled by `--speed` (3600 turns an hour into a
  second), reports the planned activities as one final event (optionally varied by `--variance`,
  from a seeded random generator so a run repeats exactly), and goes back to IDLE.
- The simulator uses only the public `MES` methods, the same ones a real cell's integration uses.
- The **Rust edge agent** follows in the next MES release and publishes to the same topics.

## Alternatives considered

- **Rust agent first:** slower to a working demo; the contract would be tested late.
- **Simulating inside the ERP:** would break the rule that the contract is the only link.

## Consequences

- `docker compose up` gives a full demo: release an order in the ERP and watch it be produced,
  confirmed and posted, with variances when `SIM_VARIANCE` is set.
- The simulator's random generator is not for secrets (bandit B311 is marked `nosec` with that
  reason).
