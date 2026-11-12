# 0005. A Rust edge agent with the standard library only, talking Modbus TCP to the PLC

- **Status:** Accepted, 2026-11-12 (MES 0.2.0)

## Context

ERP ADR 0009 planned a Rust edge agent per machine for the second MES release. An edge agent runs
on small industrial PCs next to machines, for years, often where updates are hard: it must be
small, predictable and safe with untrusted network input. It also needs a machine to talk to.

The development environment for this release could compile Rust but not download crates.

## Decision

- **Rust, standard library only.** No crates: the MQTT 3.1.1 client, the JSON parser and writer,
  Modbus TCP, ISO 8601 timestamps and logging are written in the crate (`edge/src`), each a few
  hundred lines with tests taken from the specifications (remaining-length table, CONNECT bytes,
  Modbus exception codes, every calendar day from 1600 to 2400).
- **Modbus TCP to the PLC**, the most widespread PLC protocol. A **simulated PLC** (`plc-sim`, same
  crate) exposes a register map (command, target, state, good, scrap, run seconds) and makes parts
  at a configurable cycle time, speed and scrap rate. The agent cannot tell it from a real one.
- **Plain MQTT, no TLS.** The standard library has no TLS; `mqtts://` is refused rather than
  silently downgraded. The agent connects to a broker on the same host or network (or a local TLS
  tunnel). Credentials in the URL are supported and never logged.
- **Safety with untrusted input:** packet size limit (1 MiB), JSON nesting limit (32), every Modbus
  length checked, no `unsafe` code, no panics on bad input (bad messages are logged and skipped).
- **Quality gates in CI:** `cargo fmt --check`, `cargo clippy -D warnings` with `missing_docs`
  (the documentation rule, enforced by the compiler), unit tests, and the end-to-end test against a
  real Mosquitto. A small MQTT broker in `edge/tests/common` runs the same end-to-end test without
  Mosquitto.

## Alternatives considered

- **rumqttc, serde, tokio, tokio-modbus:** what most Rust teams would use, less code to own. They
  could not be compiled or tested here, so the first build would have been in CI; and each brings
  dependencies to audit and update for years. The `Bus` and `Machine` traits in `agent.rs` keep the
  door open: a crate-based client can replace `mqtt.rs` without touching the agent's logic.
- **OPC UA instead of Modbus:** richer (types, browsing, security), but a large protocol to
  implement; Modbus is still what most machines speak.
- **The PLC simulated inside the agent:** simpler, but no real device protocol on the wire.

## Consequences

- The binaries are about 0.5 MB each and start instantly; the image holds nothing but them.
- `cargo audit` has nothing to check: there is no dependency tree.
- The protocol code is part of the project to maintain; the specifications are stable (MQTT 3.1.1
  since 2014, Modbus since 1979), and the tests pin the byte formats.
