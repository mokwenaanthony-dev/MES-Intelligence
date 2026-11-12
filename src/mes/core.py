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

**Cells with an edge agent** (``dispatch_cells``, MES 0.2.0, ADR 0006): the MES dispatches one job at
a time to ``<cell>/schedule/current-job`` (retained) and listens to ``<cell>/state/job``. When the
agent reports the job COMPLETE, the MES sends the ERP an operation-performance event with the counted
quantities and the measured machine hours, marks the request DONE and dispatches the next one. The
event ID is derived from the job (``<cell>-<order>-<operation>``), so a repeated COMPLETE, or a
restart half way through, can never post the work twice.
"""
import json
import logging
import threading
import uuid
from datetime import datetime, timedelta, timezone
from decimal import Decimal

from . import contract
from .store import Store

logger = logging.getLogger(__name__)

RESEND_AFTER = timedelta(seconds=30)


class MES:
    """One MES instance over a broker and a store. ``clock`` is injectable so tests control time."""

    def __init__(self, broker, store: Store, clock=None, dispatch_cells=()):
        self.broker = broker
        self.store = store
        self.clock = clock or (lambda: datetime.now(timezone.utc))
        self.dispatch_cells = set(dispatch_cells)
        # What each edge cell was last told to run (job ID, or None for "nothing"); unknown until the
        # first dispatch after a start, so a restarted MES publishes the current job once again.
        self._dispatched = {}
        # MQTT callbacks and the main loop both dispatch; one at a time, or two requests could be
        # started at the same cell. Re-entrant, because the in-memory test broker calls back at once.
        self._dispatch_lock = threading.RLock()

    def start(self):
        """Subscribe to the ERP's messages. Retained ones (equipment, requests) arrive at once."""
        self.broker.subscribe(contract.EQUIPMENT_SUBSCRIPTION, self._safely(self.on_equipment))
        self.broker.subscribe(contract.REQUEST_SUBSCRIPTION, self._safely(self.on_request))
        self.broker.subscribe(contract.RESULT_SUBSCRIPTION, self._safely(self.on_result))
        if self.dispatch_cells:
            self.broker.subscribe(contract.JOB_PROGRESS_SUBSCRIPTION, self._safely(self.on_job_progress))
        logger.info("MES listening to the Unified Namespace%s",
                    f"; edge cells: {', '.join(sorted(self.dispatch_cells))}" if self.dispatch_cells else "")

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
        cell = contract.cell_path(topic)
        if message is None:
            self.store.delete_request(topic)
            logger.info("Request %s cleared by the ERP", topic.rsplit("/", 1)[-1])
        elif message.get("schema") == contract.OPERATION_REQUEST:
            self.store.save_request(topic, cell, message["data"])
            logger.info("Queued %s at %s", topic.rsplit("/", 1)[-1], cell)
        else:
            self._unknown(topic, message)
            return
        if cell in self.dispatch_cells:
            self.dispatch(cell)  # new work for an idle cell, or the running job was cancelled

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

    # --- edge cells (MES 0.2.0) -------------------------------------------------------------------

    def dispatch(self, cell: str):
        """Make ``<cell>/schedule/current-job`` say which job the cell's edge agent should run.

        The running request stays the job until the agent reports it complete; otherwise the oldest
        queued request is started. With nothing to do the topic is cleared. Only a change is
        published, so calling this again and again is harmless.
        """
        with self._dispatch_lock:
            running = self.store.queue(cell, status="RUNNING")
            request = running[0] if running else next(iter(self.store.queue(cell)), None)
            if request is not None and not running:
                self.store.set_request_status(request["topic"], "RUNNING")
            job_id = request["topic"].rsplit("/", 1)[-1] if request else None
            if cell in self._dispatched and self._dispatched[cell] == job_id:
                return
            if request is None:
                payload = b""
            else:
                payload = contract.envelope(contract.JOB, {
                    "job_id": job_id, "order": request["order"], "operation_number": request["operation_number"],
                    "quantity": request["quantity"], "unit": request.get("unit"), "material": request.get("material"),
                    "planned_activities": request.get("planned_activities", {}),
                }, at=self.clock())
            self.broker.publish(contract.current_job_topic(cell), payload, retain=True)
            self._dispatched[cell] = job_id
            logger.info("Cell %s: %s", cell, f"dispatched job {job_id}" if job_id else "no job")

    def dispatch_all(self):
        """Dispatch every edge cell (the main loop calls this every few seconds)."""
        for cell in sorted(self.dispatch_cells):
            self.dispatch(cell)

    def on_job_progress(self, topic, payload):
        """An edge agent's report (``job-progress/v1``). COMPLETE becomes the ERP's performance event."""
        message = contract.parse(payload)
        if message is None:
            return
        if message.get("schema") != contract.JOB_PROGRESS:
            self._unknown(topic, message)
            return
        cell, data = contract.cell_path(topic), message["data"]
        if cell not in self.dispatch_cells:
            return  # another MES's edge cell
        if data.get("status") != "COMPLETE":
            logger.debug("Job %s at %s: %s good so far", data.get("job_id"), cell, data.get("good_quantity"))
            return
        with self._dispatch_lock:
            job_id = str(data.get("job_id") or "")
            request = next((r for r in self.store.queue(cell, status="RUNNING")
                            if r["topic"].rsplit("/", 1)[-1] == job_id), None)
            if request is None:
                # Already reported (a repeated or retained COMPLETE), or never dispatched by this MES.
                logger.info("Job %s at %s complete again: already reported, ignored", job_id, cell)
                return
            run_hours = Decimal(str(data["run_hours"]))
            # The PLC measures machine time; the cell's operator attends it for the same time, so every
            # activity planned for the operation (machine and labour) gets the measured hours.
            activities = {code: run_hours for code in request.get("planned_activities", {})} or None
            self.report_performance(
                cell, request, yield_quantity=Decimal(str(data["good_quantity"])),
                scrap_quantity=Decimal(str(data["scrap_quantity"])), activities=activities, final=True,
                event_id=f"{cell.rsplit('/', 1)[-1]}-{job_id}")
            self.store.set_request_status(request["topic"], "DONE")
            logger.info("Job %s at %s complete: %s good, %s scrap, %s h", job_id, cell, data["good_quantity"],
                        data["scrap_quantity"], run_hours)
            self.dispatch(cell)

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
                           activities: dict | None = None, final: bool = False, event_id: str | None = None) -> str:
        """Record and send one ``operation-performance/v1`` event. Returns its event ID.

        Without ``event_id`` a new one is made. With one that was already recorded, the stored bytes
        are sent again unchanged, so a repeated report is the same event to the ERP.
        """
        if event_id is not None:
            existing = self.store.event(event_id)
            if existing is not None:
                logger.info("Event %s was already recorded; sending it again unchanged", event_id)
                self._send(event_id, existing["topic"], bytes(existing["payload"]))
                return event_id
        else:
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
