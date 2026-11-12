"""What ERP-Intelligence publishes, built exactly as in contract v1 (ERP ``docs/UNS_CONTRACT.md``).

These are the contract's own examples, so a test that passes here proves the MES understands what
the ERP sends. If the contract changes, these change first.
"""
import json

PRESS = "1000/1000/assy/l1/press3"
REQUEST_TOPIC = f"{PRESS}/schedule/operation-requests/mo00000001-0010"


def envelope(schema, data, event_id=None):
    """A message in the contract's envelope, as the ERP's source sends it."""
    return json.dumps({"schema": schema, "event_id": event_id, "timestamp": "2026-06-01T08:00:00+02:00",
                       "source": "erp-intelligence", "data": data}).encode()


EQUIPMENT = envelope("equipment/v1", {
    "company_code": "1000", "plant": "1000", "area": "ASSY", "line": "L1", "work_center": "PRESS3",
    "name": "Press 3", "isa95_level": "WorkCell", "cost_center": "4000", "activity_types": ["LAB", "MACH"],
    "is_active": True, "uns_path": PRESS,
})

REQUEST = envelope("operation-request/v1", {
    "order": "MO00000001", "operation_number": 10, "description": "Press frame", "material": "SF-3001",
    "material_description": "Pressed frame", "quantity": "100.000", "unit": "EA", "start_date": "2026-06-01",
    "finish_date": "2026-06-03", "status": "RELEASED", "planned_activities": {"LAB": "10.000", "MACH": "10.500"},
}, "erp-outbox-17")


def result(event_id, status="POSTED", reason=None, replayed=False):
    """The ERP's answer to one event (confirmation-result/v1)."""
    return envelope("confirmation-result/v1", {
        "event_id": event_id, "status": status, "order": "MO00000001",
        "confirmation_number": "0001" if status == "POSTED" else None,
        "journal_entry": "PC00000001" if status == "POSTED" else None, "reason": reason, "replayed": replayed,
    }, event_id)
