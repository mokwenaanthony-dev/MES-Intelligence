# 0007. A value is only as good as its freshness: sessions, a heartbeat, frozen values and a checked contract

- **Status:** Accepted, 2026-11-23 (MES 0.2.1)

## Context

Volkan Alkılıç's post "The device died. Your dashboard still says 72.4 °C" (November 2026) sets out
the problem: a retained MQTT message keeps a device's last value on screen after the device is gone,
and a frozen value looks exactly like a steady one. A last will tells subscribers the client is
offline; Sparkplug B goes further and ties the death notice (NDEATH) to the session that announced
the values (NBIRTH, `bdSeq`), so a host marks everything stale without per-tag logic. The comments
add the harder case: a live connection reading a stuck sensor. The protocol calls it fresh; only
checking the reading itself catches it.

Reading the post against MES 0.2.0 found a real failure, not a theoretical one:

- The edge agent's OFFLINE last will was built **once, at start-up**, so its timestamp was the
  start-up time. When the agent died hours later, the broker published a message hours older than
  the agent's last report. ERP-Intelligence, whose rule is "an older report never overwrites a newer
  one", discarded it: a dead cell stayed RUNNING in the ERP, on its graph and (by not being raised)
  in its control tower. The broker's retained topic was right; the ERP's copy was frozen. The ERP
  now accepts an OFFLINE from the source of the current state whatever its timestamp (ERP v0.23.1,
  ERP ADR 0011 amended), but a will of an earlier connection could still overrule a current report.
- With report by exception, a steady cell sends nothing, so nobody can tell it from a silent one.
- A PLC whose registers stop changing while it says RUNNING would be reported RUNNING forever.

Separately, ERP-Intelligence now states the contract as JSON Schema files and checks what it
publishes (ERP ADR 0021); the MES's side of that check was still missing.

## Decision

- **One session per connection** (the bdSeq idea in contract v1's optional `session` field). Before
  every connection attempt the agent names a new session (`<cell>-<connection time>`) and builds its
  last will afresh, stamped and named for that connection. Every state report carries the same
  session. A consumer can then tell the will of an earlier connection from the current one, and the
  ERP does (v0.23.1).
- **A heartbeat:** an unchanged cell state is published again every `--heartbeat-s` (default 60 s),
  with the same `since` and a new `timestamp`. A consumer can judge freshness by the age of the
  retained state: older than a few heartbeats means the publisher is not there. Only the change is
  logged. Zero turns it off (report by exception only).
- **Frozen values are not trusted:** a RUNNING PLC always counts run time, so a reading whose
  counters (good, scrap, run seconds) have not moved for `--stall-after-s` (default 60 s) is frozen.
  The agent reports the cell OFFLINE (the contract's "we cannot see this cell") with one warning,
  and RUNNING again as soon as a counter moves. It never restarts the job for it.
- **The contract is checked from the MES side too.** `contract/erp-uns/v1/` is a pinned copy of the
  ERP's schema files at the release named in `PINNED`, verified against the ERP's `SHA256SUMS`.
  `tests/test_contract_schemas.py` checks the ERP messages the tests use, everything the Python MES
  publishes to the ERP (and that it adds no undocumented field), and the edge agent's messages.
- **The Rust crate keeps the standard library only** (ADR 0005), so it has no JSON Schema library.
  Its check is a chain: `edge/tests/golden.rs` proves the agent writes exactly the shape of the files
  in `edge/tests/golden/` (only the timestamp, `since` and `session` may differ, as strings), and the
  Python test checks those files against the schemas. A change to what the agent writes fails the
  first link until the files change; a file that breaks the contract fails the second.

## Alternatives considered

- **Sparkplug B at the edge:** the protocol has exactly these mechanisms (NBIRTH, NDEATH, bdSeq,
  rebirth), but its topic shape hides the ISA-95 paths the business layer uses (ERP ADR 0007), and
  its Protobuf payloads would need a library or a hand-written encoder. The session field brings its
  key idea into plain MQTT and JSON.
- **A quality field on every state** (OPC UA style, value + timestamp + quality): the cleanest for
  consumers, as Carsten Dyhr-Nielsen comments under the post. It is a change to the ERP contract that
  the ERP must agree first; OFFLINE already says "not trustworthy" for a cell, so it can wait.
- **Detecting frozen values by learning each signal's normal movement** (another comment): right for
  analogue sensors. A machine's run-time counter must move while it runs, so here the rule is exact.
- **A heartbeat topic of its own:** keeps the state topic quiet, but every consumer would have to join
  two topics; repeating the state keeps "the retained state's age is its freshness".

## Consequences

- A dead agent is reported OFFLINE once the broker misses its keep-alive (1.5 × 30 s); a stuck PLC
  after `--stall-after-s`; a silent publisher shows as an old retained state. The north star's
  machine-state target (< 5 s) holds for changes, not for detecting a dead link: that takes up to
  45 s unless the keep-alive is shortened.
- The ERP sees each heartbeat as unchanged and writes nothing; marking a state stale from its age is
  the ERP's next step, now that the heartbeat exists.
- Neither side can change the contract alone: the ERP checks what it publishes against its files, and
  the MES checks what it publishes against the same files, pinned.
- Not yet: the Python MES's simulated cells have no last will (one client publishes several cells, and
  a will covers one topic), so if the MES dies their states stay as they were; the heartbeat makes
  that visible by age. A quality field waits for a contract change.
