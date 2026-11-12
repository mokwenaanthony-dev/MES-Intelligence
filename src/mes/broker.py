"""Connections to the MQTT broker: Mosquitto through paho-mqtt, or an in-memory stand-in for tests.

The same small interface as ERP-Intelligence's ``apps/uns/broker.py`` (``publish``, ``subscribe``,
``close``), written again here because the two repositories share no code (ADR 0001).
"""
import logging
from urllib.parse import unquote, urlparse

logger = logging.getLogger(__name__)


def topic_matches(pattern: str, topic: str) -> bool:
    """MQTT wildcard matching: ``+`` is one level, ``#`` (last) is any number of levels."""
    pattern_levels, topic_levels = pattern.split("/"), topic.split("/")
    for index, level in enumerate(pattern_levels):
        if level == "#":
            return True
        if index >= len(topic_levels) or (level != "+" and level != topic_levels[index]):
            return False
    return len(pattern_levels) == len(topic_levels)


class MemoryBroker:
    """An in-process broker: retained messages, wildcards, retained delivery on subscribe."""

    def __init__(self):
        self.retained = {}
        self.published = []
        self.subscriptions = []

    def publish(self, topic, payload, retain=False):
        """Store a retained payload (empty deletes it) and deliver to every matching subscriber."""
        payload = payload if isinstance(payload, bytes) else str(payload).encode("utf-8")
        self.published.append((topic, payload, retain))
        if retain:
            if payload:
                self.retained[topic] = payload
            else:
                self.retained.pop(topic, None)
        for pattern, callback in list(self.subscriptions):
            if topic_matches(pattern, topic):
                callback(topic, payload)

    def subscribe(self, pattern, callback):
        """Register ``callback(topic, payload)`` and send it the matching retained messages."""
        self.subscriptions.append((pattern, callback))
        for topic, payload in list(self.retained.items()):
            if topic_matches(pattern, topic):
                callback(topic, payload)

    def close(self):
        """Nothing to close."""


class MqttBroker:  # exercised by tests/test_mqtt.py in CI, which runs Mosquitto
    """A real MQTT connection (paho-mqtt 2.x, QoS 1). ``persistent`` keeps the broker session between
    connections, so messages for the MES are queued while it is stopped."""

    def __init__(self, url: str, client_id: str, persistent: bool = False):
        from paho.mqtt import client as mqtt

        self._mqtt = mqtt
        parts = urlparse(url)
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id=client_id,
                                  clean_session=not persistent)
        if parts.username:
            self.client.username_pw_set(unquote(parts.username), unquote(parts.password or ""))
        if parts.scheme == "mqtts":
            self.client.tls_set()
        self._patterns = []
        self.client.on_connect = self._on_connect
        port = parts.port or (8883 if parts.scheme == "mqtts" else 1883)
        self.client.connect(parts.hostname, port, keepalive=60)
        self.client.loop_start()
        logger.info("Connected to MQTT broker %s:%s as %s", parts.hostname, port, client_id)

    def _on_connect(self, client, userdata, flags, reason_code, properties):
        for pattern in self._patterns:  # subscribe again after every (re)connection
            client.subscribe(pattern, qos=1)

    def publish(self, topic, payload, retain=False):
        """Publish with QoS 1 and wait until the broker has it; raises if it cannot be sent."""
        info = self.client.publish(topic, payload, qos=1, retain=retain)
        info.wait_for_publish(timeout=10)
        if info.rc != self._mqtt.MQTT_ERR_SUCCESS or not info.is_published():
            raise ConnectionError(f"MQTT publish to {topic} failed (rc={info.rc}).")

    def subscribe(self, pattern, callback):
        """Deliver every message matching ``pattern`` to ``callback(topic, payload)`` (QoS 1)."""
        self._patterns.append(pattern)
        self.client.message_callback_add(pattern, lambda client, userdata, msg: callback(msg.topic, msg.payload))
        self.client.subscribe(pattern, qos=1)

    def close(self):
        """Stop the network loop and disconnect."""
        self.client.loop_stop()
        self.client.disconnect()
