# The ERP's UNS contract, pinned

A copy of [`contract/uns/v1/`](https://github.com/mokwenaanthony-dev/ERP-Intelligence/tree/v0.23.1/contract/uns)
from ERP-Intelligence, at the release named in `PINNED` (ADR 0007 here, ERP ADR 0021). The ERP owns
the contract; the MES never edits this folder. `tests/test_contract_schemas.py` checks:

- the copy is unchanged: every file matches `v1/SHA256SUMS` (line endings normalised to `\n`);
- the ERP messages the MES's tests feed it (`tests/erp_messages.py`) follow the contract, so the
  MES is tested with what the ERP really sends;
- everything the Python MES publishes to the ERP (`operation-performance/v1`, `equipment-state/v1`)
  follows it and sends no field the contract does not document;
- the Rust edge agent's messages, pinned in `edge/tests/golden/` by `edge/tests/golden.rs`, follow it.

**Updating to a newer ERP release:** copy the ERP's `contract/uns/v1/` over `v1/`, change `PINNED`,
and run the tests. A failing test is a contract change the MES must follow first.
