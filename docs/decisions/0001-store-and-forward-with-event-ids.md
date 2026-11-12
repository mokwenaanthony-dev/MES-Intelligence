# 0001. Store-and-forward with the event ID as the idempotency key

- **Status:** Accepted, 2026-11-12 (MES 0.1.0)

## Context

The shop floor must keep working when the ERP or the broker is down, and work reported during an
outage must reach the ERP afterwards, exactly once in effect. MQTT QoS 1 delivers *at least* once,
so the ERP can receive the same message more than once, and the MES cannot know whether a message
it sent before a crash arrived.

## Decision

- Every `operation-performance/v1` event gets an **event ID** (`<cell>-<uuid>`) when the work is
  reported. It is the idempotency key of ERP ADR 0010: the ERP stores it as the confirmation's
  `external_id`, and the same ID again returns the confirmation posted the first time.
- The MES **writes the event to its database before sending it** (`performance_events`, status
  PENDING, with the exact bytes sent).
- The event stays PENDING until a `confirmation-result/v1` with its event ID arrives (POSTED or
  REJECTED). Every 5 seconds the MES resends, **byte for byte**, each event that has had no answer
  for 30 seconds.
- The resend timing uses the MES's own clock, which tests replace with a fake one.

## Alternatives considered

- **Send and forget (QoS 0):** work done during an outage is lost.
- **Rely on QoS 1 and the broker's persistent session alone:** covers a broker restart, not an MES
  crash between doing the work and sending it, and gives no answer from the ERP.
- **A new event ID per resend:** the ERP would post the same work twice.

## Consequences

- Repeating a send any number of times has the effect of sending it once (the project's idempotency
  rule).
- A rejected event (a business rule in the ERP said no) is not resent: resending would get the same
  answer. `mes status` lists it with the reason for a person to act on.
- Resent events can arrive late and out of order; the ERP handles each on its own, so order does
  not matter.
