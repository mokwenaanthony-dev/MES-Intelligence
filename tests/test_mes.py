"""The MES against contract v1: learning cells and work from the ERP, reporting work, store-and-forward.

Every handler is also run twice to prove it is idempotent: the same message twice leaves the same
state as once (ERP ADR 0010).
"""
import json
from decimal import Decimal

import pytest

from mes import contract
from mes.broker import MemoryBroker, topic_matches
from mes.core import MES
from mes.store import Store

from .conftest import ASSY, PRESS, RESULTS
from .erp_messages import EQUIPMENT, REQUEST, REQUEST_TOPIC, result


def sent_events(broker):
    return [json.loads(p) for t, p, _ in broker.published if t.endswith("/events/operation-performance")]


# --- from the ERP ----------------------------------------------------------------------------------

def test_cells_and_work_arrive_from_retained_messages(broker, clock):
    # The ERP published before the MES started: retained messages are delivered on subscribe.
    broker.publish(f"{PRESS}/definition/equipment", EQUIPMENT, retain=True)
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    mes = MES(broker, Store(":memory:"), clock=clock)
    mes.start()
    [cell] = mes.store.equipment()
    assert (cell["uns_path"], cell["name"], cell["activity_types"]) == (PRESS, "Press 3", ["LAB", "MACH"])
    [request] = mes.store.queue(PRESS)
    assert (request["order"], request["operation_number"], request["quantity"]) == ("MO00000001", 10, "100.000")
    assert request["planned_activities"] == {"LAB": "10.000", "MACH": "10.500"}
    mes.store.close()


def test_the_same_message_twice_changes_nothing(mes, broker):
    for _ in range(2):
        broker.publish(f"{PRESS}/definition/equipment", EQUIPMENT, retain=True)
        broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    assert len(mes.store.equipment()) == 1 and len(mes.store.queue()) == 1


def test_a_running_request_keeps_its_status_when_republished(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    mes.store.set_request_status(REQUEST_TOPIC, "RUNNING")
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)  # e.g. the ERP's uns_republish
    assert mes.store.queue() == [] and len(mes.store.queue(status="RUNNING")) == 1


def test_cleared_messages_remove_work_and_cells(mes, broker):
    broker.publish(f"{PRESS}/definition/equipment", EQUIPMENT, retain=True)
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    broker.publish(REQUEST_TOPIC, b"", retain=True)                       # order completed in the ERP
    broker.publish(f"{PRESS}/definition/equipment", b"", retain=True)     # cell deleted in the ERP
    assert mes.store.queue() == [] and mes.store.equipment() == []


def test_bad_or_unknown_messages_never_stop_the_mes(mes, broker, caplog):
    broker.publish(REQUEST_TOPIC, b"not json", retain=True)
    broker.publish(f"{PRESS}/definition/equipment", contract.envelope("equipment/v9", {}), retain=True)
    assert "Could not handle message" in caplog.text
    assert "Ignored message with unknown schema 'equipment/v9'" in caplog.text
    broker.publish(RESULTS, contract.envelope("confirmation-result/v9", {"event_id": "x"}))
    assert "'confirmation-result/v9'" in caplog.text
    assert mes.store.queue() == [] and mes.store.equipment() == []
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)  # the next good message is handled as usual
    assert len(mes.store.queue()) == 1


# --- to the ERP ------------------------------------------------------------------------------------

def test_performance_is_stored_then_sent_in_the_contract_format(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    [request] = mes.store.queue(PRESS)
    event_id = mes.report_performance(PRESS, request, yield_quantity=Decimal("100"),
                                      activities={"MACH": Decimal("10.5"), "LAB": "10"}, final=True)
    [message] = sent_events(broker)
    assert message["schema"] == "operation-performance/v1" and message["source"] == "mes-intelligence"
    assert message["event_id"] == event_id and event_id.startswith("press3-") and len(event_id) <= 64
    assert message["data"] == {"order": "MO00000001", "operation_number": 10, "posting_date": "2026-06-01",
                               "yield_quantity": "100", "scrap_quantity": "0", "final": True,
                               "activities": {"MACH": "10.5", "LAB": "10"}}
    [stored] = mes.store.pending_events()
    assert stored["event_id"] == event_id and stored["attempts"] == 1


def test_the_answer_stops_resending(mes, broker, clock):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    event_id = mes.report_performance(PRESS, mes.store.queue()[0], yield_quantity=Decimal("100"))
    clock.advance(seconds=10)
    assert mes.resend_pending() == 0  # not yet: give the ERP time to answer
    clock.advance(seconds=25)
    assert mes.resend_pending() == 1  # no answer after 30 s: send the same event again
    first, second = sent_events(broker)
    assert first == second  # identical bytes, same event ID: the ERP posts it once (ADR 0010)
    broker.publish(RESULTS, result(event_id))
    broker.publish(RESULTS, result(event_id, replayed=True))  # the ERP's answer to the repeat
    clock.advance(minutes=5)
    assert mes.resend_pending() == 0
    [event] = mes.store.events()
    assert event["status"] == "POSTED" and event["result"]["replayed"] is True


def test_a_rejection_is_recorded_with_its_reason(mes, broker, caplog):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    event_id = mes.report_performance(PRESS, mes.store.queue()[0], yield_quantity=Decimal("1"))
    broker.publish(RESULTS, result(event_id, "REJECTED", "Order MO00000001 is completed and settled."))
    [event] = mes.store.events()
    assert event["status"] == "REJECTED" and "completed" in event["result"]["reason"]
    assert "REJECTED: Order MO00000001" in caplog.text


def test_answers_for_other_mes_instances_are_ignored(mes, broker):
    broker.publish(RESULTS, result("someone-elses-event"))
    broker.publish(RESULTS, b"")
    broker.publish(RESULTS, contract.envelope("something-else/v1", {}))
    assert mes.store.events() == []


def test_a_broker_outage_keeps_the_event_for_later(mes, broker, clock):
    class Down(MemoryBroker):
        def publish(self, topic, payload, retain=False):
            raise ConnectionError("broker unreachable")

    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    request = mes.store.queue()[0]
    mes.broker = Down()
    event_id = mes.report_performance(PRESS, request, yield_quantity=Decimal("100"))
    [stored] = mes.store.pending_events()
    assert stored["event_id"] == event_id and stored["attempts"] == 0  # kept, not lost
    mes.broker = broker  # the broker is back
    assert mes.resend_pending() == 1 and sent_events(broker)[0]["event_id"] == event_id


def test_cell_state_is_retained(mes, broker):
    mes.set_state(PRESS, "RUNNING", "MO00000001", 10)
    state = json.loads(broker.retained[f"{PRESS}/state/status"])
    assert state["schema"] == "equipment-state/v1"
    assert state["data"] == {"state": "RUNNING", "order": "MO00000001", "operation_number": 10,
                             "since": "2026-06-01T08:00:00+00:00"}
    with pytest.raises(ValueError, match="Unknown state"):
        mes.set_state(PRESS, "SLEEPING")


def test_status_snapshot(mes, broker):
    broker.publish(f"{PRESS}/definition/equipment", EQUIPMENT, retain=True)
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    mes.report_performance(PRESS, mes.store.queue()[0], yield_quantity=Decimal("5"))
    snapshot = mes.status()
    assert len(snapshot["cells"]) == 1 and len(snapshot["queued"]) == 1
    assert snapshot["events"] == {"PENDING": 1} and snapshot["recent_events"][0]["status"] == "PENDING"


# --- the contract module ---------------------------------------------------------------------------

@pytest.mark.parametrize("pattern, topic, expected", [
    (contract.REQUEST_SUBSCRIPTION, REQUEST_TOPIC, True),
    (contract.REQUEST_SUBSCRIPTION, f"{PRESS}/definition/equipment", False),
    (contract.RESULT_SUBSCRIPTION, RESULTS, True),
    ("1000/#", f"{ASSY}/state/status", True),
    ("a/b/c", "a/b", False),
])
def test_wildcards(pattern, topic, expected):
    assert topic_matches(pattern, topic) is expected


def test_contract_helpers():
    assert contract.cell_path(REQUEST_TOPIC) == PRESS
    assert contract.performance_topic(PRESS) == f"{PRESS}/events/operation-performance"
    assert contract.parse(b"") is None
    with pytest.raises(ValueError, match="not a contract envelope"):
        contract.parse(b"[1, 2]")
    with pytest.raises(TypeError):
        contract.envelope("x/v1", {"bad": object()})
