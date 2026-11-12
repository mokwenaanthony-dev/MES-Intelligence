# MES-Intelligence

[![MES CI](https://github.com/mokwenaanthony-dev/MES-Intelligence/actions/workflows/ci.yml/badge.svg)](https://github.com/mokwenaanthony-dev/MES-Intelligence/actions/workflows/ci.yml)

A small **manufacturing execution system** (ISA-95 Level 3) and a **Rust edge agent** (Level 2) for
the shop floor of [ERP-Intelligence](https://github.com/mokwenaanthony-dev/ERP-Intelligence). The
systems never call each other: they meet in an **ISA-95 Unified Namespace** on an MQTT broker
(Eclipse Mosquitto). The ERP and the MES share only the
[Unified Namespace contract](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/UNS_CONTRACT.md)
(**contract v1**); the MES and its edge agents share the [edge contract](docs/EDGE_CONTRACT.md).

```
Level 4  ERP-Intelligence      released order ──► .../press3/schedule/operation-requests/mo..-0010 (retained)
                               confirmation   ◄── .../press3/events/operation-performance
                               answer         ──► 1000/1000/erp/events/confirmation-result
                                                        │              ▲
Level 3  MES (Python)          queue, dispatch ─────────┘              │ store-and-forward until answered
                               current job    ──► .../press3/schedule/current-job (retained)
                               COMPLETE       ◄── .../press3/state/job (retained)
                                                        │              ▲
Level 2  Edge agent (Rust)     reconcile loop: wanted job vs PLC ──────┘ + .../press3/state/status
                                                        │ Modbus TCP   ▲
Level 1  PLC (simulated)       registers: command, target │ state, good, scrap, run seconds
```

## What it delivers

| Area | What exists | Since |
|---|---|---|
| Work queue | Subscribes to the ERP's retained `equipment/v1` and `operation-request/v1` messages, so it has the whole plan as soon as it connects. An empty retained message (order completed or cancelled) removes the request | 0.1.0 |
| Reporting work | `operation-performance/v1` events with yield, scrap and machine and labour hours; exact decimal strings, never floats | 0.1.0 |
| Store-and-forward | Every event is written to SQLite **before** it is sent and resent byte for byte until the ERP answers; the event ID is the ERP's idempotency key ([ADR 0001](docs/decisions/0001-store-and-forward-with-event-ids.md)) | 0.1.0 |
| Machine simulator | Works a cell's queue at `--speed` with optional seeded `--variance`, for cells without a machine ([ADR 0004](docs/decisions/0004-simulator-before-the-edge-agent.md)) | 0.1.0 |
| **Dispatching** | For `--dispatch` cells the MES puts one job at a time on the cell's retained `current-job` topic and turns the agent's COMPLETE into the ERP event, with the **measured** hours and counted scrap; the event ID comes from the job, so nothing is posted twice ([ADR 0006](docs/decisions/0006-the-mes-dispatches-and-the-edge-reports-state.md)) | 0.2.0 |
| **Rust edge agent** | `edge-agent` reconciles the wanted job with the PLC every half second, drives it over **Modbus TCP**, and publishes retained job progress and cell state; MQTT last will = OFFLINE; a state file makes restarts harmless ([ADR 0005](docs/decisions/0005-std-only-rust-edge-agent-over-modbus.md)) | 0.2.0 |
| **Simulated PLC** | `plc-sim`: a Modbus TCP server with a register map, a part counter, cycle time, speed and scrap rate | 0.2.0 |
| **No dependencies** | The Rust crate uses the standard library only: its own MQTT 3.1.1 client, JSON parser, Modbus TCP and ISO 8601 dates, each tested against its specification | 0.2.0 |
| Troubleshooting | Python and Rust log the same way: time, level, thread, module, file and **line number**; a bad message is logged and skipped, never stops anything | 0.1.0, 0.2.0 |
| Quality gates | Python: flake8, bandit, pip-audit, pytest (coverage at least 80 %) and a documentation test. Rust: rustfmt, clippy with warnings as errors and `missing_docs`, unit tests, end-to-end tests. Both against a real Mosquitto in CI, and both Docker images built | 0.1.0, 0.2.0 |

Next: live cell state on ERP-Intelligence's knowledge graph (the graph's `Cell` nodes take the
retained `state/status`).

## Run the full demo

You need Docker and a running ERP-Intelligence with the demo data (`python manage.py setup_demo_production`).

```bash
# 1. Here: broker, MES, and Press 3 as a real machine (simulated PLC + edge agent)
docker compose up -d
docker compose logs -f mes edge-press3 plc-press3

# 2. In ERP-Intelligence's .env (host) or Compose (containers: mqtt://host.docker.internal:1883)
MQTT_URL=mqtt://localhost:1883

# 3. In ERP-Intelligence: publish the current state, then listen for the MES's work
python manage.py uns_republish
python manage.py uns_listen
```

Release a production order in the ERP (`POST /api/production/orders/<id>/release/`). Within seconds:

1. the MES queues the operations and dispatches the PRESS3 operation to its edge agent;
2. the agent starts the PLC, which makes 100 parts in 10.5 machine hours (about 10 real seconds);
3. the agent reports COMPLETE; the MES sends the confirmation with the measured hours;
4. the ERP posts it and answers POSTED. The order's cost report
   (`GET /api/production/orders/<id>/costs/`) shows the confirmed machine and labour hours.

ASSY1 (a manual cell) is worked by the MES's simulator. Try `PLC_SCRAP_EVERY=25 docker compose up -d`
to see scrap reach the ERP, or `docker compose stop edge-press3` to see the cell go OFFLINE (the
agent's last will). Watch the whole namespace with `docker compose exec broker mosquitto_sub -v -t '#'`.

## Run it without Docker

```bash
python -m venv .venv && source .venv/Scripts/activate   # Windows Git Bash; Linux/macOS: .venv/bin/activate
pip install -r requirements-dev.txt

# terminal 1: the MES (ASSY1 simulated, PRESS3 dispatched to an edge agent)
python -m mes run --simulate 1000/1000/assy/l1/assy1 --dispatch 1000/1000/assy/l1/press3

# terminals 2 and 3: the PLC and the edge agent (needs Rust: https://rustup.rs)
cd edge
cargo run --release --bin plc-sim -- --listen 127.0.0.1:5020
cargo run --release --bin edge-agent -- --cell 1000/1000/assy/l1/press3 --plc 127.0.0.1:5020

python -m mes status          # cells, queue, running work and events, from the local database
```

| Setting | Default | |
|---|---|---|
| `MQTT_URL` / `--mqtt-url` | `mqtt://localhost:1883` | MES: also `mqtts://user:password@host:8883`; edge agent: plain `mqtt://` only (ADR 0005) |
| `MES_DB` / `--db` | `mes.sqlite3` | `/data/mes.sqlite3` in Docker |
| `EDGE_CELL`, `EDGE_PLC`, `EDGE_STATE_FILE` | (required), `localhost:5020`, `edge-state.txt` | the edge agent's cell, PLC and memory (`edge/src/bin/edge_agent.rs`) |
| `PLC_CYCLE_SECONDS`, `PLC_SPEED`, `PLC_SCRAP_EVERY` | `378`, `3600`, `0` | the simulated PLC (`edge/src/bin/plc_sim.rs`) |
| `LOG_LEVEL` | `INFO` | `DEBUG` shows every message |

The Mosquitto configuration in `mosquitto/` allows anonymous clients: **local development only**.

## Tests and checks

```bash
# Python MES
flake8 .
bandit -r src -ll
pip-audit -r requirements.txt
pytest                                    # with coverage; the Mosquitto test is skipped

# Rust edge agent
cd edge
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                # unit tests and end to end with the built-in test broker

# Both against a real broker (docker compose up -d broker first)
MQTT_URL=mqtt://localhost:1883 pytest
MQTT_URL=mqtt://localhost:1883 cargo test
```

## Project layout

```
src/mes/            the MES (Python)
  contract.py       topics, schemas and the JSON envelope (ERP contract v1, edge contract v1)
  broker.py         MemoryBroker (tests) and MqttBroker (paho-mqtt 2, persistent session)
  store.py          SQLite with plain SQL: equipment, operation requests, performance events
  core.py           from the ERP, to the ERP, resend, dispatching to edge cells
  simulator.py      a machine working its cell's queue, for cells without an edge agent
  __main__.py       the `mes` command: run, status
edge/               the edge agent and the simulated PLC (Rust, standard library only)
  src/mqtt.rs       MQTT 3.1.1 client: framing, QoS 1, keep-alive, last will
  src/json.rs       JSON value, recursive-descent parser, writer (numbers kept as exact text)
  src/modbus.rs     Modbus TCP client and server
  src/plc.rs        the simulated PLC's registers and part counter
  src/agent.rs      the reconcile loop: wanted job vs PLC, state and progress out
  src/messages.rs   the edge contract's messages
  src/time.rs       ISO 8601 from the system clock (days-to-civil-date algorithm)
  src/log.rs        log lines with file and line number
  tests/            end to end over TCP: test broker or Mosquitto, simulated PLC, agent
tests/              the ERP contract's example messages, scenarios on the in-memory broker, edge flow
docs/               the edge contract and the decision records
```

## Decisions

The [architecture decision records](docs/decisions/README.md) explain the MES's own choices. The
shared ones are in ERP-Intelligence: [the MES in its own repository](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0009-mes-in-its-own-repository.md),
[Unified Namespace conventions](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0007-unified-namespace-conventions.md)
and [idempotency](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0010-idempotency-of-every-state-changing-operation.md).

## License

[MIT](LICENSE)
