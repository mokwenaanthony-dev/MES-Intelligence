# MES-Intelligence

[![MES CI](https://github.com/mokwenaanthony-dev/MES-Intelligence/actions/workflows/ci.yml/badge.svg)](https://github.com/mokwenaanthony-dev/MES-Intelligence/actions/workflows/ci.yml)

A small **manufacturing execution system** (ISA-95 level 3) for the shop floor of
[ERP-Intelligence](https://github.com/mokwenaanthony-dev/ERP-Intelligence). The two systems never
call each other: they meet in an **ISA-95 Unified Namespace** on an MQTT broker (Eclipse Mosquitto),
and the only thing they share is the
[Unified Namespace contract](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/UNS_CONTRACT.md)
(this release implements **contract v1**).

```
ERP-Intelligence                    Mosquitto (this repository)                  MES-Intelligence
released order --> 1000/1000/assy/l1/press3/schedule/operation-requests/mo..-0010 --> work queue
work centre    --> 1000/1000/assy/l1/press3/definition/equipment ----------------> known cells
                                                                                    | simulator works it
               <-- 1000/1000/assy/l1/press3/state/equipment-state (RUNNING, IDLE) <-+
confirmation   <-- 1000/1000/assy/l1/press3/events/operation-performance <--------- work done
  (journal entry)                                                                   | stored first,
answer         --> 1000/1000/erp/events/confirmation-result (POSTED/REJECTED) -----> stops resending
```

## What 0.1.0 delivers

| Area | What exists |
|---|---|
| Work queue | Subscribes to the ERP's retained `equipment/v1` and `operation-request/v1` messages, so it has the whole current plan as soon as it connects. An empty retained message (order completed or cancelled in the ERP) removes the request |
| Reporting work | `operation-performance/v1` events with yield, scrap and machine and labour hours; exact decimal strings, never floats |
| Store-and-forward | Every event is written to SQLite **before** it is sent, and resent byte for byte until the ERP answers. The event ID is the ERP's idempotency key, so a resend never posts twice ([ADR 0001](docs/decisions/0001-store-and-forward-with-event-ids.md)) |
| Cell state | Retained `equipment-state/v1` (IDLE, RUNNING, ...), so anyone subscribing sees what every cell is doing now |
| Machine simulator | Works each cell's queue at `--speed` (3600 = one planned hour per second), optionally with `--variance` from a seeded random generator, so the ERP shows real cost variances ([ADR 0004](docs/decisions/0004-simulator-before-the-edge-agent.md)) |
| Troubleshooting | Every log line has the time, level, thread, module, function and **line number**; a bad message is logged and skipped, never stops the MES |
| Quality gates | flake8, bandit, pip-audit, pytest with an 80 % coverage floor, an integration test against a real Mosquitto, and a test that every module, class and function is documented |

Next release: a **Rust edge agent** reading a (simulated) PLC and publishing to the same topics.

## Run the full demo

You need Docker and a running ERP-Intelligence with the demo data (`python manage.py setup_demo_production`).

```bash
# 1. Here: the broker and the MES with simulated Press 3 and Assembly cell 1
docker compose up -d
docker compose logs -f mes

# 2. In ERP-Intelligence's .env (host) or Compose (containers: mqtt://host.docker.internal:1883)
MQTT_URL=mqtt://localhost:1883

# 3. In ERP-Intelligence: publish the current state, then listen for the MES's work
python manage.py uns_republish
python manage.py uns_listen
```

Release a production order in the ERP (`POST /api/production/orders/<id>/release/`). Within seconds
the MES log shows it queued, worked and reported, and the order's cost report
(`GET /api/production/orders/<id>/costs/`) shows the confirmed machine and labour hours. Start the
MES with `SIM_VARIANCE=0.1 docker compose up -d` to see variances.

Watch the whole namespace with `docker compose exec broker mosquitto_sub -v -t '#'`.

## Run it without Docker

```bash
python -m venv .venv && source .venv/Scripts/activate   # Windows Git Bash; Linux/macOS: .venv/bin/activate
pip install -r requirements-dev.txt

python -m mes run --simulate 1000/1000/assy/l1/press3 --simulate 1000/1000/assy/l1/assy1
python -m mes status          # cells, queue, running work and events, from the local database
```

| Setting | Default | |
|---|---|---|
| `MQTT_URL` / `--mqtt-url` | `mqtt://localhost:1883` | `mqtts://user:password@host:8883` for TLS and a login |
| `MES_DB` / `--db` | `mes.sqlite3` | `/data/mes.sqlite3` in Docker |
| `LOG_LEVEL` | `INFO` | `DEBUG` shows every message |

The Mosquitto configuration in `mosquitto/` allows anonymous clients: **local development only**.

## Tests and checks

```bash
flake8 .
bandit -r src -ll
pip-audit -r requirements.txt
pytest                                    # with coverage; the Mosquitto test is skipped
MQTT_URL=mqtt://localhost:1883 pytest     # with the broker running: the integration test too
```

## Project layout

```
src/mes/
  contract.py   topics, schemas and the JSON envelope of contract v1
  broker.py     MemoryBroker (tests) and MqttBroker (paho-mqtt 2, persistent session)
  store.py      SQLite with plain SQL: equipment, operation requests, performance events
  core.py       the MES: from the ERP (cells, queue, answers), to the ERP (state, performance), resend
  simulator.py  a machine working its cell's queue
  __main__.py   the `mes` command: run, status
tests/          the contract's example messages, scenarios on the in-memory broker, one on Mosquitto
docs/decisions/ why it is built this way
```

## Decisions

The [architecture decision records](docs/decisions/README.md) explain the MES's own choices. The
shared ones are in ERP-Intelligence: [the MES in its own repository](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0009-mes-in-its-own-repository.md),
[Unified Namespace conventions](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0007-unified-namespace-conventions.md)
and [idempotency](https://github.com/mokwenaanthony-dev/ERP-Intelligence/blob/main/docs/decisions/0010-idempotency-of-every-state-changing-operation.md).

## License

[MIT](LICENSE)
