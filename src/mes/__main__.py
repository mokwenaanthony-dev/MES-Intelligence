"""Command line: ``python -m mes run`` (the MES, optionally with simulated cells) and ``python -m mes status``.

Settings come from the command line or the environment:

``MQTT_URL``  broker address, default ``mqtt://localhost:1883``
``MES_DB``    SQLite file, default ``mes.sqlite3``

Examples::

    python -m mes run --simulate 1000/1000/assy/l1/press3 --simulate 1000/1000/assy/l1/assy1
    python -m mes run --simulate 1000/1000/assy/l1/assy1 --dispatch 1000/1000/assy/l1/press3
    python -m mes status

``--simulate`` cells are worked by the MES's own machine simulator; ``--dispatch`` cells have an edge
agent (``edge/`` in this repository) that runs each job on the machine and reports back.
"""
import argparse
import json
import logging
import os
import threading
import time

from .core import MES
from .simulator import MachineSimulator
from .store import Store

FORMAT = "%(asctime)s | %(levelname)-7s | %(threadName)s | %(name)s | %(module)s.%(funcName)s:%(lineno)d | %(message)s"


def build_parser() -> argparse.ArgumentParser:
    """The ``mes`` command and its sub-commands."""
    parser = argparse.ArgumentParser(prog="mes", description="MES-Intelligence on the Unified Namespace.")
    parser.add_argument("--mqtt-url", default=os.environ.get("MQTT_URL", "mqtt://localhost:1883"))
    parser.add_argument("--db", default=os.environ.get("MES_DB", "mes.sqlite3"))
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="run the MES (Ctrl+C stops it)")
    run.add_argument("--simulate", action="append", default=[], metavar="CELL_PATH",
                     help="simulate the machine at this cell, e.g. 1000/1000/assy/l1/press3 (repeatable)")
    run.add_argument("--dispatch", action="append", default=[], metavar="CELL_PATH",
                     help="dispatch jobs to the edge agent of this cell (repeatable)")
    run.add_argument("--speed", type=float, default=3600, help="simulated seconds per real second (default 3600)")
    run.add_argument("--variance", type=float, default=0.0, help="random +/- share of planned activity, e.g. 0.1")
    commands.add_parser("status", help="show cells, queue and events from the local database")
    return parser


def run(args, broker_factory=None, stop=None) -> MES:
    """Start the MES and its simulators; block until ``stop()`` is true (or Ctrl+C)."""
    if broker_factory is None:
        from .broker import MqttBroker

        def broker_factory():
            """A persistent session, so messages for the MES wait on the broker while it is stopped."""
            return MqttBroker(args.mqtt_url, client_id="mes-intelligence", persistent=True)
    both = set(args.simulate) & set(args.dispatch)
    if both:
        raise SystemExit("A cell is either simulated or dispatched to an edge agent, not both: "
                         + ", ".join(sorted(both)))
    mes = MES(broker_factory(), Store(args.db), dispatch_cells=args.dispatch)
    mes.start()
    mes.dispatch_all()
    threads = []
    for cell in args.simulate:
        simulator = MachineSimulator(mes, cell, speed=args.speed, variance=args.variance)
        thread = threading.Thread(target=simulator.run, kwargs={"stop": stop},
                                  name=f"sim-{cell.rsplit('/', 1)[-1]}", daemon=True)
        thread.start()
        threads.append(thread)
    try:
        while not (stop and stop()):
            mes.resend_pending()
            mes.dispatch_all()
            time.sleep(5)
    except KeyboardInterrupt:
        logging.getLogger(__name__).info("Stopping")
    for thread in threads:  # let a simulator finish the step it is in before the caller closes the store
        thread.join(timeout=10)
    return mes


def status(args) -> dict:
    """What the local database knows, as JSON."""
    store = Store(args.db)
    try:
        return MES(broker=None, store=store).status()  # reading needs no broker
    finally:
        store.close()


def main(argv=None):
    """Entry point of ``python -m mes``."""
    args = build_parser().parse_args(argv)
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"), format=FORMAT)
    if args.command == "status":
        print(json.dumps(status(args), indent=2))
    else:
        run(args)


if __name__ == "__main__":
    main()
