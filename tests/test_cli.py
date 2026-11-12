"""The ``python -m mes`` command line, end to end with the in-memory broker."""
import json

from mes import __main__ as cli
from mes.broker import MemoryBroker

from .erp_messages import REQUEST, REQUEST_TOPIC


def test_run_with_a_simulated_press_then_status(tmp_path, capsys):
    db = str(tmp_path / "mes.sqlite3")
    broker = MemoryBroker()
    broker.publish(REQUEST_TOPIC, REQUEST, retain=True)
    args = cli.build_parser().parse_args(["--db", db, "run", "--simulate", "1000/1000/assy/l1/press3",
                                          "--speed", "1e9"])
    checks = iter(range(400))

    def stop():
        """Stop once the simulated press has reported, or after enough checks."""
        return any(t.endswith("operation-performance") for t, _, _ in broker.published) or next(checks, None) is None

    cli.time.sleep = lambda seconds: None  # the main loop's 5 s pause
    mes = cli.run(args, broker_factory=lambda: broker, stop=stop)
    mes.store.close()
    assert any(t.endswith("operation-performance") for t, _, _ in broker.published)

    cli.main(["--db", db, "status"])
    snapshot = json.loads(capsys.readouterr().out)
    assert snapshot["events"] == {"PENDING": 1}


def test_settings_come_from_the_environment(monkeypatch):
    monkeypatch.setenv("MQTT_URL", "mqtt://plant-broker:1883")
    monkeypatch.setenv("MES_DB", "/data/mes.sqlite3")
    args = cli.build_parser().parse_args(["run"])
    assert (args.mqtt_url, args.db, args.simulate, args.speed) == ("mqtt://plant-broker:1883", "/data/mes.sqlite3",
                                                                   [], 3600)
