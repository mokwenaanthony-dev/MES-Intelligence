# 0003. A local SQLite database, written with plain SQL

- **Status:** Accepted, 2026-11-12 (MES 0.1.0)

## Context

The MES needs memory that survives a restart (the work queue and the unanswered events of
ADR 0001), on a small shop-floor machine, without a database server to run.

## Decision

- **SQLite** (in the standard library), one file (`MES_DB`, `/data/mes.sqlite3` in Docker).
- **Plain SQL in one module**, `store.py`, with no ORM: three tables, every write an upsert on the
  message's natural key (cell path, request topic, event ID), so receiving a message twice leaves the
  same rows as receiving it once.
- One connection in autocommit mode, guarded by one lock, because MQTT messages arrive on the
  client's network thread while the simulator and the resend loop run on others.

## Alternatives considered

- **PostgreSQL:** a server to run and secure on every site, for a few thousand rows.
- **SQLAlchemy or the Django ORM:** a large dependency for three tables, and it would hide the SQL a
  learner should see (ERP-Intelligence already shows the ORM).
- **Files or in-memory only:** no safe concurrent writes, or nothing left after a crash.

## Consequences

- Tests use `Store(":memory:")`: fast, and nothing to clean up.
- One writer at a time is enough for one MES per site; a busier site would move to PostgreSQL behind
  the same `Store` methods.
