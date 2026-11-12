"""The Unified Namespace contract, version 1, as seen from the MES.

Mirrors ``apps/uns/contract.py`` in ERP-Intelligence: the same topic paths, schema names and
envelope. The ERP repository owns the contract (``docs/UNS_CONTRACT.md``); this module pins
version 1 and the tests check it against the examples there.
"""
import json
from datetime import datetime, timezone
from decimal import Decimal

SOURCE = "mes-intelligence"
EQUIPMENT = "equipment/v1"
OPERATION_REQUEST = "operation-request/v1"
OPERATION_PERFORMANCE = "operation-performance/v1"
CONFIRMATION_RESULT = "confirmation-result/v1"
EQUIPMENT_STATE = "equipment-state/v1"

# What the MES listens to (any enterprise, site, area, line and cell).
EQUIPMENT_SUBSCRIPTION = "+/+/+/+/+/definition/equipment"
REQUEST_SUBSCRIPTION = "+/+/+/+/+/schedule/operation-requests/+"
RESULT_SUBSCRIPTION = "+/+/erp/events/confirmation-result"

STATES = ("RUNNING", "IDLE", "SETUP", "DOWN", "OFFLINE")


def cell_path(topic: str) -> str:
    """The cell part of an equipment-level topic: the first five levels."""
    return "/".join(topic.split("/")[:5])


def performance_topic(cell: str) -> str:
    """Where a cell reports work done (not retained)."""
    return f"{cell}/events/operation-performance"


def state_topic(cell: str) -> str:
    """Where a cell's current state is kept (retained)."""
    return f"{cell}/state/status"


def _default(value):
    if isinstance(value, Decimal):
        return format(value, "f")  # exact decimal strings, as the contract requires
    raise TypeError(f"{type(value).__name__} is not JSON serialisable")


def envelope(schema: str, data: dict, event_id: str | None = None, at: datetime | None = None) -> bytes:
    """A message in the contract's envelope, as UTF-8 JSON bytes."""
    message = {"schema": schema, "event_id": event_id,
               "timestamp": (at or datetime.now(timezone.utc)).isoformat(), "source": SOURCE, "data": data}
    return json.dumps(message, default=_default).encode("utf-8")


def parse(payload: bytes) -> dict | None:
    """The envelope of a message, or None for an empty payload (a cleared retained topic).

    Raises ``ValueError`` for anything that is not a JSON envelope with a ``data`` object.
    """
    if not payload:
        return None
    message = json.loads(payload)
    if not isinstance(message, dict) or not isinstance(message.get("data"), dict):
        raise ValueError("not a contract envelope")
    return message
