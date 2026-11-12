"""The machine simulator: works its cell's queue and reports what it did, in simulated time."""
import json
from decimal import Decimal

from mes.simulator import MachineSimulator

from .conftest import ASSY, PRESS
from .erp_messages import REQUEST, REQUEST_TOPIC, result


def test_a_simulated_press_works_the_order_and_reports_it(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    slept = []
    simulator = MachineSimulator(mes, PRESS, speed=3600, sleep=slept.append)
    event_id = simulator.step()
    assert slept == [10.5]  # the longest planned activity, 10.5 hours, at 3600x = 10.5 s
    [request] = mes.store.queue(PRESS, status="DONE")
    assert request["order"] == "MO00000001"
    [message] = [json.loads(p) for t, p, _ in broker.published if t.endswith("operation-performance")]
    assert message["event_id"] == event_id
    assert message["data"]["activities"] == {"LAB": "10.000", "MACH": "10.500"}  # as planned
    assert message["data"]["yield_quantity"] == "100.000" and message["data"]["final"] is True
    states = [json.loads(p)["data"]["state"] for t, p, _ in broker.published if t.endswith("state/status")]
    assert states == ["RUNNING", "IDLE"]
    assert simulator.step() is None  # nothing left at this cell


def test_variance_is_repeatable(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    MachineSimulator(mes, PRESS, variance=0.1, seed=7, sleep=lambda s: None).step()
    first = [json.loads(p)["data"]["activities"] for t, p, _ in broker.published if t.endswith("performance")]
    assert first[0] != {"LAB": "10.000", "MACH": "10.500"}
    assert all(Decimal("9") <= Decimal(v) <= Decimal("11.6") for v in first[0].values())
    again = MachineSimulator(mes, PRESS, variance=0.1, seed=7, sleep=lambda s: None)
    # Same seed, same draws in the same order (labour, then machine): the same numbers.
    assert [again._actual(Decimal("10.000")), again._actual(Decimal("10.500"))] == [
        Decimal(first[0]["LAB"]), Decimal(first[0]["MACH"])]


def test_run_works_the_queue_then_resends_until_answered(mes, broker, clock):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    loops = iter(range(3))
    simulator = MachineSimulator(mes, PRESS, sleep=lambda s: clock.advance(seconds=31))
    simulator.run(stop=lambda: next(loops, None) is None)
    events = [json.loads(p)["event_id"] for t, p, _ in broker.published if t.endswith("performance")]
    assert len(events) >= 2 and len(set(events)) == 1  # one event, resent while unanswered
    broker.publish("1000/1000/erp/events/confirmation-result", result(events[0]))
    assert mes.store.pending_events() == []


def test_another_cells_work_is_left_alone(mes, broker):
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    assert MachineSimulator(mes, ASSY, sleep=lambda s: None).step() is None
    assert len(mes.store.queue(PRESS)) == 1
