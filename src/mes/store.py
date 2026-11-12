"""The MES's own memory: a small SQLite database, written with plain SQL.

Three tables:

``equipment``           what each cell is, from the ERP's retained ``equipment/v1`` messages
``operation_requests``  the work queue: one row per operation of a released order, at its cell,
                        from the ERP's retained ``operation-request/v1`` messages
``performance_events``  every ``operation-performance/v1`` event the MES sent, until the ERP has
                        answered it: the store-and-forward outbox

Every write is an upsert keyed by the message's natural key (cell path, request topic, event ID),
so receiving a message twice leaves the same rows as receiving it once (idempotent, ERP ADR 0010).
The MQTT client delivers messages on its own thread, so one lock guards the single connection.
"""
import json
import sqlite3
import threading
from datetime import datetime, timezone

SCHEMA = """
CREATE TABLE IF NOT EXISTS equipment (
    uns_path    TEXT PRIMARY KEY,
    data        TEXT NOT NULL,              -- the equipment/v1 data object, as JSON
    updated_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS operation_requests (
    topic             TEXT PRIMARY KEY,     -- .../schedule/operation-requests/<order>-<op>
    cell              TEXT NOT NULL,        -- the cell's UNS path
    order_number      TEXT NOT NULL,
    operation_number  INTEGER NOT NULL,
    data              TEXT NOT NULL,        -- the operation-request/v1 data object, as JSON
    status            TEXT NOT NULL DEFAULT 'QUEUED' CHECK (status IN ('QUEUED', 'RUNNING', 'DONE')),
    updated_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS operation_requests_cell ON operation_requests (cell, status);
CREATE TABLE IF NOT EXISTS performance_events (
    event_id      TEXT PRIMARY KEY,         -- the idempotency key the ERP stores as external_id
    topic         TEXT NOT NULL,
    payload       BLOB NOT NULL,            -- the exact bytes sent, so a resend is identical
    status        TEXT NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'POSTED', 'REJECTED')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_sent_at  TEXT,
    result        TEXT,                     -- the confirmation-result/v1 data object, as JSON
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS performance_events_status ON performance_events (status, last_sent_at);
"""


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


class Store:
    """All database access of the MES. ``path=":memory:"`` gives a throwaway database for tests."""

    def __init__(self, path: str = "mes.sqlite3"):
        self._lock = threading.Lock()
        self._db = sqlite3.connect(path, check_same_thread=False, isolation_level=None)  # autocommit
        self._db.row_factory = sqlite3.Row
        with self._lock:
            self._db.executescript(SCHEMA)

    def _execute(self, sql, params=()):
        with self._lock:
            return self._db.execute(sql, params)

    def _query(self, sql, params=()):
        with self._lock:
            return [dict(row) for row in self._db.execute(sql, params).fetchall()]

    # --- equipment ---------------------------------------------------------------------------------

    def save_equipment(self, uns_path: str, data: dict):
        """Insert or replace a cell's definition."""
        self._execute("INSERT INTO equipment (uns_path, data, updated_at) VALUES (?, ?, ?) "
                      "ON CONFLICT (uns_path) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
                      (uns_path, json.dumps(data), _now()))

    def delete_equipment(self, uns_path: str):
        """Forget a cell the ERP no longer has."""
        self._execute("DELETE FROM equipment WHERE uns_path = ?", (uns_path,))

    def equipment(self) -> list[dict]:
        """Every known cell, by path."""
        return [{"uns_path": r["uns_path"], **json.loads(r["data"])}
                for r in self._query("SELECT uns_path, data FROM equipment ORDER BY uns_path")]

    # --- the work queue ----------------------------------------------------------------------------

    def save_request(self, topic: str, cell: str, data: dict):
        """Insert a request, or update its details while keeping how far the shop floor has got with it."""
        self._execute(
            "INSERT INTO operation_requests (topic, cell, order_number, operation_number, data, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (topic) DO UPDATE SET data = excluded.data, "
            "updated_at = excluded.updated_at",
            (topic, cell, data["order"], int(data["operation_number"]), json.dumps(data), _now()))

    def delete_request(self, topic: str):
        """The ERP cleared the request (order completed or cancelled): drop it from the queue."""
        self._execute("DELETE FROM operation_requests WHERE topic = ?", (topic,))

    def set_request_status(self, topic: str, status: str):
        """Move a request to QUEUED, RUNNING or DONE."""
        self._execute("UPDATE operation_requests SET status = ?, updated_at = ? WHERE topic = ?",
                      (status, _now(), topic))

    def queue(self, cell: str | None = None, status: str = "QUEUED") -> list[dict]:
        """Requests in a status, oldest start date first (then order and operation)."""
        sql = "SELECT topic, cell, status, data FROM operation_requests WHERE status = ?"
        params = [status]
        if cell:
            sql += " AND cell = ?"
            params.append(cell)
        rows = self._query(sql, params)
        requests = [{"topic": r["topic"], "cell": r["cell"], "status": r["status"], **json.loads(r["data"])}
                    for r in rows]
        return sorted(requests, key=lambda r: (r["start_date"], r["order"], r["operation_number"]))

    # --- store-and-forward -------------------------------------------------------------------------

    def save_event(self, event_id: str, topic: str, payload: bytes, at: str | None = None):
        """Keep an outgoing event until the ERP answers it. Saving it again changes nothing."""
        self._execute("INSERT INTO performance_events (event_id, topic, payload, created_at) VALUES (?, ?, ?, ?) "
                      "ON CONFLICT (event_id) DO NOTHING", (event_id, topic, payload, at or _now()))

    def mark_sent(self, event_id: str, at: str | None = None):
        """Count an attempt to send the event, at ``at`` (ISO time from the MES's clock; default now)."""
        self._execute("UPDATE performance_events SET attempts = attempts + 1, last_sent_at = ? WHERE event_id = ?",
                      (at or _now(), event_id))

    def record_result(self, event_id: str, status: str, result: dict) -> bool:
        """Store the ERP's answer. Returns False for an event this MES did not send."""
        cursor = self._execute("UPDATE performance_events SET status = ?, result = ? WHERE event_id = ?",
                               (status, json.dumps(result), event_id))
        return cursor.rowcount == 1

    def pending_events(self, sent_before: str | None = None) -> list[dict]:
        """Events still waiting for an answer, optionally only those last sent before a time."""
        sql, params = "SELECT * FROM performance_events WHERE status = 'PENDING'", []
        if sent_before:
            sql += " AND (last_sent_at IS NULL OR last_sent_at < ?)"
            params.append(sent_before)
        return self._query(sql + " ORDER BY created_at", params)

    def events(self) -> list[dict]:
        """Every event with its status and answer, newest first (for ``mes status``)."""
        rows = self._query("SELECT event_id, topic, status, attempts, result, created_at FROM performance_events "
                           "ORDER BY created_at DESC")
        for row in rows:
            row["result"] = json.loads(row["result"]) if row["result"] else None
        return rows

    def close(self):
        """Close the database."""
        with self._lock:
            self._db.close()
