"""Edge cells (MES 0.2.0): the MES dispatches jobs to an edge agent and reports its results to the ERP.

The edge agent's messages are built here exactly as the Rust agent writes them (``edge/src/messages.rs``),
so these tests pin the Level 2/3 contract (``docs/EDGE_CONTRACT.md``) from the Python side.
"""
import json
import logging
from decimal import Decimal

import pytest

from mes import contract
from mes.core import MES
from mes.store import Store

from .conftest import ASSY, PRESS
from .erp_messages import REQUEST, REQUEST_TOPIC, envelope, result

SECOND_TOPIC = f"{PRESS}/schedule/operation-requests/mo00000002-0010"
SECOND = envelope("operation-request/v1", {
    "order": "MO00000002", "operation_number": 10, "description": "Press frame", "material": "SF-3001",
    "material_description": "Pressed frame", "quantity": "50.000", "unit": "EA", "start_date": "2026-06-02",
    "finish_date": "2026-06-03", "status": "RELEASED", "planned_activities": {"LAB": "5.000", "MACH": "5.250"},
})


def progress(job_id, status="COMPLETE", good="100", scrap="2", hours="10.750", order="MO00000001"):
    """A ``job-progress/v1`` message as the Rust edge agent writes it."""
    return json.dumps({"schema": "job-progress/v1", "event_id": None, "timestamp": "2026-06-01T18:45:00.000+00:00",
                       "source": "mes-edge/press3",
                       "data": {"job_id": job_id, "order": order, "operation_number": 10, "status": status,
                                "good_quantity": good, "scrap_quantity": scrap, "run_hours": hours}}).encode()


@pytest.fixture
def edge_mes(broker, clock):
    instance = MES(broker, Store(":memory:"), clock=clock, dispatch_cells=[PRESS])
    instance.start()
    yield instance
    instance.store.close()


def current_job(broker):
    """The retained current-job message for PRESS3, decoded (None when cleared)."""
    payload = broker.retained.get(contract.current_job_topic(PRESS))
    return json.loads(payload) if payload else None


def performance_events(broker):
    return [json.loads(p) for t, p, _ in broker.published if t.endswith("/events/operation-performance")]


def test_released_work_is_dispatched_one_job_at_a_time(edge_mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    broker.publish(SECOND_TOPIC, SECOND, retain=True)
    job = current_job(broker)
    assert job["schema"] == "job/v1"
    assert job["data"] == {"job_id": "mo00000001-0010", "order": "MO00000001", "operation_number": 10,
                           "quantity": "100.000", "unit": "EA", "material": "SF-3001",
                           "planned_activities": {"LAB": "10.000", "MACH": "10.500"}}
    assert [r["order"] for r in edge_mes.store.queue(PRESS, status="RUNNING")] == ["MO00000001"]
    assert [r["order"] for r in edge_mes.store.queue(PRESS)] == ["MO00000002"]  # waits its turn


def test_complete_becomes_one_performance_event_then_the_next_job(edge_mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    broker.publish(SECOND_TOPIC, SECOND, retain=True)
    broker.publish(contract.job_progress_topic(PRESS), progress("mo00000001-0010", status="RUNNING", good="40"),
                   retain=True)
    assert performance_events(broker) == []  # progress alone posts nothing

    broker.publish(contract.job_progress_topic(PRESS), progress("mo00000001-0010"), retain=True)
    [event] = performance_events(broker)
    assert event["event_id"] == "press3-mo00000001-0010"  # derived from the job: the same every time
    assert event["data"] == {"order": "MO00000001", "operation_number": 10, "posting_date": "2026-06-01",
                             "yield_quantity": "100", "scrap_quantity": "2", "final": True,
                             "activities": {"LAB": "10.750", "MACH": "10.750"}}  # measured, not planned
    assert edge_mes.store.queue(PRESS, status="DONE")[0]["order"] == "MO00000001"
    assert current_job(broker)["data"]["job_id"] == "mo00000002-0010"


def test_a_repeated_complete_posts_nothing_more(edge_mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    for _ in range(3):  # QoS 1 redelivery, or the retained copy after the MES reconnects
        broker.publish(contract.job_progress_topic(PRESS), progress("mo00000001-0010"), retain=True)
    assert len(performance_events(broker)) == 1
    assert current_job(broker) is None  # nothing left to run: the topic is cleared


def test_a_restarted_mes_reports_the_same_event_unchanged(broker, clock, tmp_path):
    db = str(tmp_path / "mes.sqlite3")
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    first = MES(broker, Store(db), clock=clock, dispatch_cells=[PRESS])
    first.start()
    [request] = first.store.queue(PRESS, status="RUNNING")
    # The first life recorded and sent the event, then died before marking the request DONE.
    first.report_performance(PRESS, request, yield_quantity=Decimal("100"), final=True,
                             event_id="press3-mo00000001-0010")
    first.store.close()
    sent_first = broker.published[-1][1]

    clock.advance(minutes=5)  # a different time: a rebuilt message would differ
    second = MES(broker, Store(db), clock=clock, dispatch_cells=[PRESS])
    second.start()
    broker.publish(contract.job_progress_topic(PRESS), progress("mo00000001-0010", good="100", scrap="0"),
                   retain=True)
    events = [p for t, p, _ in broker.published if t.endswith("/events/operation-performance")]
    assert events == [sent_first, sent_first]  # the same bytes, so the same event to the ERP
    assert len(second.store.events()) == 1
    broker.publish("1000/1000/erp/events/confirmation-result", result("press3-mo00000001-0010"))
    assert second.store.events()[0]["status"] == "POSTED"
    second.store.close()


def test_the_erp_cancelling_the_running_order_moves_on(edge_mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    broker.publish(SECOND_TOPIC, SECOND, retain=True)
    broker.publish(REQUEST_TOPIC, b"", retain=True)  # order MO00000001 cancelled in the ERP
    assert current_job(broker)["data"]["job_id"] == "mo00000002-0010"
    broker.publish(SECOND_TOPIC, b"", retain=True)
    assert current_job(broker) is None


def test_dispatch_publishes_only_changes(edge_mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    count = len(broker.published)
    edge_mes.dispatch_all()
    edge_mes.dispatch_all()
    assert len(broker.published) == count


def test_progress_from_other_cells_and_bad_messages_are_ignored(edge_mes, broker, caplog):
    caplog.set_level(logging.INFO)
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    broker.publish(contract.job_progress_topic(ASSY), progress("mo00000001-0010"), retain=True)  # not an edge cell
    broker.publish(contract.job_progress_topic(PRESS), progress("mo99999999-0010"), retain=True)  # not dispatched
    broker.publish(contract.job_progress_topic(PRESS), b"", retain=True)  # agent cleared its state
    broker.publish(contract.job_progress_topic(PRESS), contract.envelope("job-progress/v9", {}), retain=True)
    assert performance_events(broker) == []
    assert "complete again: already reported" in caplog.text
    assert "unknown schema 'job-progress/v9'" in caplog.text


def test_simulated_cells_are_not_dispatched(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    assert contract.current_job_topic(PRESS) not in broker.retained
    [request] = mes.store.queue(PRESS)
    assert (request["status"], request["order_status"]) == ("QUEUED", "RELEASED")  # the MES's and the ERP's
