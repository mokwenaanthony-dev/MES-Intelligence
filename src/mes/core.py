"""The central MES: listens to the ERP, keeps the work queue, reports work done.

``MES.start()`` subscribes to three things in the Unified Namespace (contract v1):

* ``<cell>/definition/equipment`` (retained, from the ERP): which cells exist;
* ``<cell>/schedule/operation-requests/<order>-<op>`` (retained, from the ERP): the work queue. An
  empty payload means the ERP completed or cancelled the order, and the request leaves the queue;
* ``<enterprise>/<site>/erp/events/confirmation-result``: the ERP's answer to each event.

``report_performance`` records an event in the store **before** sending it, then publishes it.
Until the ERP answers, ``resend_pending`` sends the same bytes again with the same event ID: the ERP
treats the repeat as the same event (ERP ADR 0010), so nothing is charged twice and nothing is lost
while the ERP or the broker is down (store-and-forward, contract section 2).
"""
import json
import logging
import uuid
from datetime import datetime, timedelta, timezone
from decimal import Decimal

from . import contract
from .store import Store

logger = logging.getLogger(__name__)

RESEND_AFTER = timedelta(seconds=30)


class MES:
    """One MES instance over a broker and a store. ``clock`` is injectable so tests control time."""

    def __init__(self, broker, store: Store, clock=None):
        self.broker = broker
        self.store = store
        self.clock = clock or (lambda: datetime.now(timezone.utc))

    def start(self):
        """Subscribe to the ERP's messages. Retained ones (equipment, requests) arrive at once."""
        self.broker.subscribe(contract.EQUIPMENT_SUBSCRIPTION, self._safely(self.on_equipment))
        self.broker.subscribe(contract.REQUEST_SUBSCRIPTION, self._safely(self.on_request))
        self.broker.subscribe(contract.RESULT_SUBSCRIPTION, self._safely(self.on_result))
        logger.info("MES listening to the Unified Namespace")

    @staticmethod
    def _safely(handler):
        """A bad message is logged and skipped; it must never stop the MES."""
        def wrapped(topic, payload):
            """Call ``handler`` and log, instead of raise, anything that goes wrong."""
            try:
                handler(topic, payload)
            except Exception:
                logger.exception("Could not handle message on %s", topic)
        return wrapped

    @staticmethod
    def _unknown(topic, message):
        """A schema this MES does not know (perhaps from a newer ERP): log it and carry on (contract v1)."""
        logger.warning("Ignored message with unknown schema %r on %s", message.get("schema"), topic)

    # --- from the ERP ------------------------------------------------------------------------------

    def on_equipment(self, topic, payload):
        """Store or forget a cell definition (``equipment/v1``)."""
        cell = contract.cell_path(topic)
        message = contract.parse(payload)
        if message is None:
            self.store.delete_equipment(cell)
            logger.info("Cell %s removed", cell)
        elif message.get("schema") == contract.EQUIPMENT:
            self.store.save_equipment(cell, message["data"])
            logger.info("Cell %s: %s", cell, message["data"].get("name"))
        else:
            self._unknown(topic, message)

    def on_request(self, topic, payload):
        """Queue a released operation (``operation-request/v1``), or drop it when the ERP clears it."""
        message = contract.parse(payload)
        if message is None:
            self.store.delete_request(topic)
            logger.info("Request %s cleared by the ERP", topic.rsplit("/", 1)[-1])
        elif message.get("schema") == contract.OPERATION_REQUEST:
            self.store.save_request(topic, contract.cell_path(topic), message["data"])
            logger.info("Queued %s at %s", topic.rsplit("/", 1)[-1], contract.cell_path(topic))
        else:
            self._unknown(topic, message)

    def on_result(self, topic, payload):
        """Record the ERP's answer (``confirmation-result/v1``); once answered, an event is not resent."""
        message = contract.parse(payload)
        if message is None:
            return
        if message.get("schema") != contract.CONFIRMATION_RESULT:
            self._unknown(topic, message)
            return
        data = message["data"]
        if self.store.record_result(data.get("event_id") or "", data.get("status", "REJECTED"), data):
            level = logging.INFO if data.get("status") == "POSTED" else logging.WARNING
            logger.log(level, "Event %s %s%s", data.get("event_id"), data.get("status"),
                       f": {data.get('reason')}" if data.get("reason") else "")

    # --- to the ERP --------------------------------------------------------------------------------

    def set_state(self, cell: str, state: str, order: str | None = None, operation_number: int | None = None):
        """Publish what a cell is doing now (retained ``equipment-state/v1``)."""
        if state not in contract.STATES:
            raise ValueError(f"Unknown state {state}; use one of {', '.join(contract.STATES)}.")
        data = {"state": state, "order": order, "operation_number": operation_number,
                "since": self.clock().isoformat()}
        self.broker.publish(contract.state_topic(cell), contract.envelope(contract.EQUIPMENT_STATE, data,
                                                                          at=self.clock()), retain=True)

    def report_performance(self, cell: str, request: dict, *, yield_quantity: Decimal, scrap_quantity=Decimal("0"),
                           activities: dict | None = None, final: bool = False) -> str:
        """Record and send one ``operation-performance/v1`` event. Returns its event ID."""
        event_id = f"{cell.rsplit('/', 1)[-1]}-{uuid.uuid4().hex}"
        data = {"order": request["order"], "operation_number": request["operation_number"],
                "posting_date": self.clock().date().isoformat(),
                "yield_quantity": Decimal(yield_quantity), "scrap_quantity": Decimal(scrap_quantity),
                "final": final}
        if activities is not None:
            data["activities"] = {code: Decimal(quantity) for code, quantity in activities.items()}
        topic = contract.performance_topic(cell)
        payload = contract.envelope(contract.OPERATION_PERFORMANCE, data, event_id, at=self.clock())
        self.store.save_event(event_id, topic, payload, at=self.clock().isoformat())  # first keep it, then send it
        self._send(event_id, topic, payload)
        return event_id

    def _send(self, event_id, topic, payload):
        try:
            self.broker.publish(topic, payload)
            self.store.mark_sent(event_id, at=self.clock().isoformat())
        except Exception as exc:  # broker down: the event stays pending and is resent later
            logger.warning("Could not send %s (%s); it will be resent", event_id, exc)

    def resend_pending(self) -> int:
        """Send again every event that has had no answer for ``RESEND_AFTER``. Returns how many."""
        cutoff = (self.clock() - RESEND_AFTER).isoformat()
        events = self.store.pending_events(sent_before=cutoff)
        for event in events:
            self._send(event["event_id"], event["topic"], bytes(event["payload"]))
        if events:
            logger.info("Resent %d unanswered event(s)", len(events))
        return len(events)

    def status(self) -> dict:
        """A snapshot for people: cells, queue, running work and events by status."""
        events = self.store.events()
        counts = {}
        for event in events:
            counts[event["status"]] = counts.get(event["status"], 0) + 1
        return {"cells": self.store.equipment(), "queued": self.store.queue(),
                "running": self.store.queue(status="RUNNING"), "events": counts,
                "recent_events": [json.loads(json.dumps(e, default=str)) for e in events[:10]]}
