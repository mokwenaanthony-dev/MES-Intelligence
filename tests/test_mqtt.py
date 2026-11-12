"""The MES against a real Mosquitto. Skipped unless MQTT_URL is set (CI starts a broker)."""
import os
import queue
import time
import uuid
from decimal import Decimal

import pytest

from mes.broker import MqttBroker
from mes.core import MES
from mes.store import Store

from .erp_messages import REQUEST

MQTT_URL = os.environ.get("MQTT_URL", "")
pytestmark = pytest.mark.skipif(not MQTT_URL, reason="MQTT_URL not set: no broker to test against")


def test_work_arrives_and_performance_leaves_through_mosquitto():
    root = f"t{uuid.uuid4().hex[:6]}"  # a private enterprise segment, so parallel runs never collide
    cell = f"{root}/1000/assy/l1/press3"
    erp = MqttBroker(MQTT_URL, client_id=f"erp-{root}")
    mes_broker = MqttBroker(MQTT_URL, client_id=f"mes-{root}")
    store = Store(":memory:")
    try:
        received = queue.Queue()
        erp.subscribe(f"{root}/+/+/+/+/events/operation-performance", lambda t, p: received.put(p))
        erp.publish(f"{cell}/schedule/operation-requests/mo00000001-0010", REQUEST, retain=True)

        mes = MES(mes_broker, store)
        mes.start()
        for _ in range(50):  # the retained request arrives on the network thread
            if mes.store.queue(cell):
                break
            time.sleep(0.1)
        [request] = mes.store.queue(cell)
        event_id = mes.report_performance(cell, request, yield_quantity=Decimal("100"))
        assert event_id.encode() in received.get(timeout=10)
    finally:
        erp.publish(f"{cell}/schedule/operation-requests/mo00000001-0010", b"", retain=True)
        erp.close()
        mes_broker.close()
        store.close()
