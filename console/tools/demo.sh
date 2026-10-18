#!/usr/bin/env bash
# Starts orbitd and the console with realistic demo data and waits; Ctrl-C stops and cleans up everything.
#
#   console/tools/demo.sh                     # then open http://127.0.0.1:18081/ (console token: demo)
#   console/tools/demo.sh --shots DIR         # take light and dark screenshots of every screen into DIR, then stop
#
# The data: seven apps (jobs, a service, two consumers), two versions of `etl` with KV data, a failing job, a consumer whose
# messages die in a dead-letter queue, and 36 hours of runs, spans and log lines written straight into orbitd's database by
# backfill.py (a dev-only helper) so that the charts have a history. Needs cargo, the orin CLI (ORIN), python3; the screenshots
# need node and Playwright (PLAYWRIGHT=/path/to/node_modules/playwright, CHROMIUM=/path/to/chrome).
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$here/.." && pwd)"
ORIN="${ORIN:-orin}"
ORBITD_PORT="${ORBITD_PORT:-18080}"
CONSOLE_PORT="${CONSOLE_PORT:-18081}"
CLIENT_DIR="${CLIENT_DIR:-$here/.orin/packages/orin-ui/client}"
TOKEN=demo
SHOTS=""
if [ "${1:-}" = "--shots" ]; then SHOTS="${2:?--shots needs a directory}"; fi
tmp="$(mktemp -d)"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

echo "== build"
(cd "$root" && cargo build --release --quiet -p orbitd -p occtl)
(cd "$here" && "$ORIN" fetch >/dev/null && "$ORIN" build --quiet)
for f in hello report failing service consumer consumer_fail; do
  (cd "$here/tools/demo" && "$ORIN" build --quiet --target=wasm32-orbit "$f.or" -o "$tmp/$f.wasm" && rm -f "$tmp/$f.wasm.fingerprint")
done
export ORBIT_CLOUD_URL="http://127.0.0.1:$ORBITD_PORT" ORBIT_CLOUD_TOKEN="$TOKEN"
occtl="$root/target/release/occtl"

echo "== orbitd"
"$root/target/release/orbitd" --listen "127.0.0.1:$ORBITD_PORT" --data-dir "$tmp/data" >"$tmp/orbitd.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 50); do curl -fs "http://127.0.0.1:$ORBITD_PORT/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

echo "== apps"
"$occtl" deploy hello "$tmp/hello.wasm" >/dev/null
"$occtl" deploy report "$tmp/report.wasm" >/dev/null
"$occtl" deploy etl "$tmp/report.wasm" --kv cfg --kv seen >/dev/null
"$occtl" deploy etl "$tmp/report.wasm" --kv cfg --kv seen --memory-mb 128 --timeout-ms 30000 --env REGION=eu-west --env BATCH=500 --arg=--full >/dev/null
"$occtl" deploy sync-upstream "$tmp/failing.wasm" --timeout-ms 5000 --connect "upstream.example.com:443" >/dev/null
printf '{"kind":"service","restart":"on-failure"}' > "$tmp/ticker.json"
"$occtl" deploy ticker "$tmp/service.wasm" --spec "$tmp/ticker.json" >/dev/null
"$occtl" deploy mailer "$tmp/consumer.wasm" --kind consumer --queue emails >/dev/null
printf '{"kind":"consumer","queue":"orders","max_attempts":2}' > "$tmp/orders.json"
"$occtl" deploy orders-worker "$tmp/consumer_fail.wasm" --spec "$tmp/orders.json" >/dev/null
"$occtl" kv put etl cfg settings '{"batch": 500, "retries": 3, "regions": ["eu-west", "us-east"], "dry_run": false}' >/dev/null
"$occtl" kv put etl cfg schedule '0 3 * * *' >/dev/null
"$occtl" kv put etl cfg owner 'data-platform' >/dev/null
"$occtl" kv put etl seen 'order:10231' '2026-10-17T21:14:07Z' >/dev/null
"$occtl" kv put etl seen 'order:10232' '2026-10-17T21:15:41Z' >/dev/null
echo "== history"
python3 "$here/tools/backfill.py" "$tmp/data/orbit-cloud.db" --hours 36

echo "== live runs"
for _ in 1 2 3; do "$occtl" run hello >/dev/null; "$occtl" run report >/dev/null; done
"$occtl" run etl >/dev/null
"$occtl" run sync-upstream >/dev/null || true
for i in 1 2 3; do "$occtl" queue send emails "{\"to\": \"user$i@example.com\", \"template\": \"welcome\"}" >/dev/null; done
for i in 1 2 3; do "$occtl" queue send orders "{\"order\": $((1000 + i)), \"total\": $((i * 17))}" >/dev/null; done
for _ in $(seq 1 60); do
  curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$ORBITD_PORT/v1/queues" | grep -q '"dead":3' && break
  sleep 0.25
done
echo "== console"
printf '%s' "$TOKEN" > "$tmp/token"
"$here/build/dev/orbit-console" --listen "127.0.0.1:$CONSOLE_PORT" --api "$ORBIT_CLOUD_URL" --api-token-file "$tmp/token" \
  --assets "$here/assets" --client-dir "$CLIENT_DIR" --quiet >"$tmp/console.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 50); do curl -fs "http://127.0.0.1:$CONSOLE_PORT/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
kill -0 "${pids[1]}" 2>/dev/null || { cat "$tmp/console.log"; exit 1; }

if [ -n "$SHOTS" ]; then
  mkdir -p "$SHOTS"
  CONSOLE_URL="http://127.0.0.1:$CONSOLE_PORT" CONSOLE_TOKEN="$TOKEN" OCCTL="$occtl" node "$here/tools/screenshots.js" "$SHOTS"
  exit 0
fi
echo
echo "Console: http://127.0.0.1:$CONSOLE_PORT/  (token: $TOKEN)    orbitd: $ORBIT_CLOUD_URL"
echo "Ctrl-C to stop."
wait "${pids[1]}"
