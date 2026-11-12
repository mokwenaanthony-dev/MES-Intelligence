"""Shared test fixtures: an in-memory broker, a throwaway database and a controllable clock."""
from datetime import datetime, timedelta, timezone

import pytest

from mes.broker import MemoryBroker
from mes.core import MES
from mes.store import Store

PRESS = "1000/1000/assy/l1/press3"
ASSY = "1000/1000/assy/l1/assy1"
RESULTS = "1000/1000/erp/events/confirmation-result"


class Clock:
    """A clock the test moves forward by hand."""

    def __init__(self):
        self.now = datetime(2026, 6, 1, 8, 0, tzinfo=timezone.utc)

    def __call__(self):
        return self.now

    def advance(self, **kwargs):
        """Move time forward, e.g. ``clock.advance(seconds=31)``."""
        self.now += timedelta(**kwargs)


@pytest.fixture
def clock():
    return Clock()


@pytest.fixture
def broker():
    return MemoryBroker()


@pytest.fixture
def mes(broker, clock):
    instance = MES(broker, Store(":memory:"), clock=clock)
    instance.start()
    yield instance
    instance.store.close()
