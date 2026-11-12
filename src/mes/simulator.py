"""A machine simulator: one cell working through its queue, standing in for a real machine.

For each queued request at its cell, the simulator:

1. publishes the cell state RUNNING and marks the request RUNNING;
2. "works" for the planned activity time, scaled by ``speed`` (``speed=3600`` turns one planned hour
   into one second), so a demo order finishes in seconds;
3. reports the work as one final ``operation-performance`` event: the full quantity as yield and the
   planned activities, optionally varied by ``variance`` (e.g. 0.1 = up to 10 % more or less, from a
   seeded random generator so a run can be repeated exactly);
4. marks the request DONE and the cell IDLE.

The next MES release replaces it with a Rust edge agent reading a real (or simulated) PLC, publishing
to the same topics; nothing else changes.
"""
import logging
import random
import time
from decimal import ROUND_HALF_UP, Decimal

logger = logging.getLogger(__name__)

THOUSANDTH = Decimal("0.001")


class MachineSimulator:
    """Works the queue of one cell (its UNS path) through an ``MES``."""

    def __init__(self, mes, cell: str, speed: float = 3600, variance: float = 0.0, seed: int = 1,
                 sleep=time.sleep):
        self.mes, self.cell, self.speed = mes, cell, speed
        self.variance = variance
        # A seeded generator makes a simulated run repeatable; it never produces secrets.
        self.rng = random.Random(seed)  # nosec B311
        self.sleep = sleep

    def _actual(self, planned: Decimal) -> Decimal:
        if not self.variance:
            return planned
        factor = Decimal(str(1 + self.rng.uniform(-self.variance, self.variance)))
        return (planned * factor).quantize(THOUSANDTH, rounding=ROUND_HALF_UP)

    def step(self) -> str | None:
        """Work the next queued request at this cell. Returns the event ID, or None if the queue is empty."""
        queue = self.mes.store.queue(self.cell)
        if not queue:
            return None
        request = queue[0]
        self.mes.store.set_request_status(request["topic"], "RUNNING")
        self.mes.set_state(self.cell, "RUNNING", request["order"], request["operation_number"])
        planned = {code: Decimal(qty) for code, qty in request.get("planned_activities", {}).items()}
        hours = max(planned.values(), default=Decimal("0"))  # machine and labour run side by side
        logger.info("%s working on %s/%04d for %s planned hours", self.cell, request["order"],
                    request["operation_number"], hours)
        self.sleep(float(hours) * 3600 / self.speed)
        event_id = self.mes.report_performance(
            self.cell, request, yield_quantity=Decimal(request["quantity"]),
            activities={code: self._actual(qty) for code, qty in planned.items()}, final=True)
        self.mes.store.set_request_status(request["topic"], "DONE")
        self.mes.set_state(self.cell, "IDLE")
        return event_id

    def run(self, poll_seconds: float = 2.0, stop=None):
        """Keep working: take queued requests and resend unanswered events, until ``stop()`` is true."""
        self.mes.set_state(self.cell, "IDLE")
        while not (stop and stop()):
            if self.step() is None:
                self.mes.resend_pending()
                self.sleep(poll_seconds)
