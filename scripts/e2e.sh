#!/usr/bin/env bash
# End to end across both languages: Mosquitto, the Python MES, the Rust edge agent and the simulated
# PLC, started with Docker Compose. Plays the ERP's part with mosquitto_pub/sub:
#
#   1. publishes the contract's example operation request for PRESS3 (retained, as the ERP does);
#   2. waits for the MES's operation-performance event (the ERP would post it as a confirmation);
#   3. checks it: 100 good parts, 10.5 measured hours, the job's deterministic event ID.
#
# Used by CI (docker job); run it locally with `bash scripts/e2e.sh` (it stops the stack afterwards).
set -euo pipefail
cd "$(dirname "$0")/.."

CELL=1000/1000/assy/l1/press3
REQUEST_TOPIC=$CELL/schedule/operation-requests/mo00000001-0010
OUT=$(mktemp)
trap 'docker compose down -v >/dev/null 2>&1 || true; rm -f "$OUT"' EXIT

docker compose up -d --build --wait
echo "Stack is up; publishing the operation request"

# Listen first (in the background), then publish, so the event cannot be missed.
docker compose exec -T broker mosquitto_sub -t '+/+/+/+/+/events/operation-performance' -C 1 -W 90 > "$OUT" &
LISTENER=$!
sleep 2
docker compose exec -T broker mosquitto_pub -r -q 1 -t "$REQUEST_TOPIC" -s < scripts/e2e_request.json  # -s: the message is stdin

if ! wait "$LISTENER"; then
  echo "No operation-performance event within 90 s. Logs:"
  docker compose logs --tail 40 mes edge-press3 plc-press3
  exit 1
fi
echo "Event received:"
cat "$OUT"

python3 - "$OUT" <<'PY'
import json, sys
event = json.load(open(sys.argv[1]))
data = event["data"]
checks = {
    "schema": (event["schema"], "operation-performance/v1"),
    "event_id": (event["event_id"], "press3-mo00000001-0010"),
    "yield": (data["yield_quantity"], "100"),
    "scrap": (data["scrap_quantity"], "0"),
    "machine hours": (data["activities"]["MACH"], "10.500"),
    "labour hours": (data["activities"]["LAB"], "10.500"),
    "final": (data["final"], True),
}
failed = {name: got for name, (got, wanted) in checks.items() if got != wanted}
if failed:
    sys.exit(f"Unexpected values: {failed}")
print("End to end OK: ERP request -> MES dispatch -> edge agent -> PLC -> MES -> operation-performance")
PY

# Clear the retained request, as the ERP does when the order completes.
docker compose exec -T broker mosquitto_pub -r -q 1 -t "$REQUEST_TOPIC" -n
