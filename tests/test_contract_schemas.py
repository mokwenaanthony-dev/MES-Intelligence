"""The ERP's UNS contract, checked from the MES side (MES 0.2.1, ADR 0007; ERP ADR 0021).

ERP-Intelligence states contract v1 as JSON Schema files and checks everything it publishes against
them. This module is the other half: the MES keeps a pinned copy (``contract/erp-uns/v1/``) and checks

* the copy is the ERP's, unchanged (``SHA256SUMS``);
* the ERP messages its own tests use are real contract messages;
* everything the Python MES publishes to the ERP follows the contract and adds no undocumented field;
* the Rust edge agent's state messages (pinned by ``edge/tests/golden.rs``) follow it too.

So neither side can change the contract alone: a change shows up as a failing test in the
repository that did not follow it.
"""
import hashlib
import json
from decimal import Decimal
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator
from referencing import Registry, Resource

from mes import contract
from mes.core import MES
from mes.store import Store

from . import erp_messages
from .conftest import PRESS
from .test_edge import progress

ROOT = Path(__file__).resolve().parents[1]
PINNED = ROOT / "contract" / "erp-uns"
SCHEMAS = PINNED / "v1"
GOLDEN = ROOT / "edge" / "tests" / "golden"


def _load():
    """Every pinned schema by message type (``equipment-state/v1``), and a registry for their references."""
    schemas, resources = {}, []
    for path in sorted(SCHEMAS.glob("*.schema.json")):
        schema = json.loads(path.read_text(encoding="utf-8"))
        resources.append((schema["$id"], Resource.from_contents(schema)))
        if path.name != "envelope.schema.json":
            schemas[schema["properties"]["schema"]["const"]] = schema
    return schemas, Registry().with_resources(resources)


SCHEMA_BY_TYPE, REGISTRY = _load()


def problems(payload: bytes) -> list[str]:
    """Every way a message breaks the pinned contract; empty when it follows it."""
    message = json.loads(payload)
    schema = SCHEMA_BY_TYPE.get(message.get("schema"))
    if schema is None:
        return [f"unknown schema {message.get('schema')!r}"]
    validator = Draft202012Validator(schema, registry=REGISTRY, format_checker=Draft202012Validator.FORMAT_CHECKER)
    return [f"{'/'.join(map(str, e.absolute_path)) or '(message)'}: {e.message}"
            for e in validator.iter_errors(message)]


def follows_the_contract(payload: bytes) -> dict:
    """Assert a payload follows its schema and sends only documented ``data`` fields; return it parsed."""
    assert problems(payload) == []
    message = json.loads(payload)
    documented = set(SCHEMA_BY_TYPE[message["schema"]]["properties"]["data"]["properties"])
    undocumented = set(message["data"]) - documented
    assert not undocumented, f"{message['schema']} sends fields the contract does not document: {undocumented}"
    return message


def test_the_pinned_copy_is_the_erps_unchanged():
    lines = (SCHEMAS / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
    assert len(lines) == len([p for p in SCHEMAS.rglob("*.json")])  # nothing added, nothing missing
    for line in lines:
        digest, name = line.split("  ", 1)
        data = (SCHEMAS / name).read_bytes().replace(b"\r\n", b"\n")
        assert hashlib.sha256(data).hexdigest() == digest, f"{name} differs from the pinned ERP contract"
    assert (PINNED / "PINNED").read_text(encoding="utf-8").startswith("ERP-Intelligence v")


def test_the_pin_covers_every_message_the_mes_knows():
    assert set(SCHEMA_BY_TYPE) == {contract.EQUIPMENT, contract.OPERATION_REQUEST, contract.OPERATION_PERFORMANCE,
                                   contract.CONFIRMATION_RESULT, contract.EQUIPMENT_STATE}
    assert SCHEMA_BY_TYPE[contract.EQUIPMENT_STATE]["properties"]["data"]["properties"]["state"]["enum"] == \
        list(contract.STATES)


@pytest.mark.parametrize("payload", [
    erp_messages.EQUIPMENT, erp_messages.REQUEST, erp_messages.result("press3-1"),
    erp_messages.result("press3-2", status="REJECTED", reason="Order MO00000001 is not released."),
    erp_messages.result("press3-3", replayed=True),
], ids=["equipment", "request", "posted", "rejected", "replayed"])
def test_the_erp_messages_the_tests_use_are_real_contract_messages(payload):
    follows_the_contract(payload)


@pytest.mark.parametrize("path", sorted((SCHEMAS / "examples").glob("*.json")), ids=lambda p: p.name)
def test_the_erps_own_examples_follow_the_pinned_schemas(path):
    follows_the_contract(path.read_bytes())


def published_to_the_erp(broker):
    """Everything the MES published on topics of the ERP contract (its Level 2/3 edge messages aside)."""
    return [payload for topic, payload, _ in broker.published
            if topic.endswith(("/events/operation-performance", "/state/status")) and payload]


def test_everything_the_mes_publishes_to_the_erp_follows_the_contract(mes, broker):
    broker.publish(erp_messages.REQUEST_TOPIC, erp_messages.REQUEST, retain=True)
    request = mes.store.queue()[0]
    mes.set_state(PRESS, "RUNNING", "MO00000001", 10)
    mes.report_performance(PRESS, request, yield_quantity=Decimal("60"), scrap_quantity=Decimal("1"))
    mes.report_performance(PRESS, request, yield_quantity=Decimal("40"), activities={"MACH": "4.2", "LAB": "4"},
                           final=True)
    mes.set_state(PRESS, "IDLE")
    for state in ("SETUP", "DOWN", "OFFLINE"):
        mes.set_state(PRESS, state)
    messages = [follows_the_contract(p) for p in published_to_the_erp(broker)]
    assert {m["schema"] for m in messages} == {contract.EQUIPMENT_STATE, contract.OPERATION_PERFORMANCE}
    assert len(messages) == 7  # five states and two performance events


def test_what_the_mes_reports_for_an_edge_cell_follows_the_contract(broker, clock):
    """An edge cell's COMPLETE becomes the ERP's operation-performance, with measured hours."""
    instance = MES(broker, Store(":memory:"), clock=clock, dispatch_cells=[PRESS])
    instance.start()
    try:
        broker.publish(erp_messages.REQUEST_TOPIC, erp_messages.REQUEST, retain=True)
        broker.publish(contract.job_progress_topic(PRESS), progress("mo00000001-0010"), retain=True)
        [event] = [follows_the_contract(p) for t, p, _ in broker.published
                   if t.endswith("/events/operation-performance")]
        assert event["data"]["final"] is True and event["data"]["activities"]
    finally:
        instance.store.close()


@pytest.mark.parametrize("path", sorted(GOLDEN.glob("*.json")), ids=lambda p: p.name)
def test_the_edge_agents_messages_follow_the_contract(path):
    message = follows_the_contract(path.read_bytes())
    assert message["source"].startswith("mes-edge/") and message["data"]["session"]


def test_the_edge_agent_has_pinned_its_state_messages_and_its_will():
    assert {p.name for p in GOLDEN.glob("*.json")} >= {"equipment-state-running.json", "equipment-state-idle.json",
                                                       "equipment-state-last-will.json"}


@pytest.mark.parametrize("change, problem", [
    ({"yield_quantity": 100}, "100 is not of type 'string'"),  # a JSON number, not a decimal string
    ({"posting_date": "01/06/2026"}, "posting_date"),
    ({"operation_number": "10"}, "operation_number"),
])
def test_a_message_that_breaks_the_contract_is_caught(change, problem):
    message = json.loads((SCHEMAS / "examples" / "operation-performance.json").read_bytes())
    message["data"].update(change)
    assert any(problem in line for line in problems(json.dumps(message).encode()))
