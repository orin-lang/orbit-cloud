#!/usr/bin/env bash
# End-to-end check of the console against a real orbitd, natively. Starts orbitd (127.0.0.1:18080, a temporary data directory) and
# the console (127.0.0.1:18081), deploys a few apps, checks the pages with curl and, when Playwright is installed, drives the
# console in a browser. Both servers are stopped on exit.
#
#   console/tools/e2e.sh
#   ORIN=/path/to/orin PLAYWRIGHT=/path/to/node_modules/playwright CHROMIUM=/path/to/chrome console/tools/e2e.sh
#
# Needs: cargo (or built target/release/{orbitd,occtl}), the orin CLI, curl. Optional: node + Playwright for the browser part.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$here/.." && pwd)"
ORIN="${ORIN:-orin}"
ORBITD_PORT="${ORBITD_PORT:-18080}"
CONSOLE_PORT="${CONSOLE_PORT:-18081}"
TOKEN="secret"
CONSOLE_TOKEN="console-secret"
tmp="$(mktemp -d)"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
fails=0
check() { # check DESCRIPTION COMMAND...
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then echo "ok: $desc"; else echo "FAIL: $desc"; fails=$((fails + 1)); fi
}
has() { grep -q -- "$1"; } # has PATTERN < text

echo "== build"
(cd "$root" && cargo build --release --quiet -p orbitd -p occtl)
(cd "$here" && "$ORIN" fetch >/dev/null && "$ORIN" build --quiet)
export ORBIT_CLOUD_URL="http://127.0.0.1:$ORBITD_PORT" ORBIT_CLOUD_TOKEN="$TOKEN"
occtl="$root/target/release/occtl"

echo "== orbitd"
ORBIT_CLOUD_TOKEN="$TOKEN" "$root/target/release/orbitd" --listen "127.0.0.1:$ORBITD_PORT" --data-dir "$tmp/data" >"$tmp/orbitd.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 50); do curl -fs "http://127.0.0.1:$ORBITD_PORT/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

fixture="$root/crates/orbitd/tests/fixtures/hello.wasm"
"$occtl" deploy hello "$fixture" --kv cfg >/dev/null
"$occtl" deploy hello "$fixture" --kv cfg >/dev/null          # a second version, to roll back to
"$occtl" kv put hello cfg color blue >/dev/null
"$occtl" kv put hello cfg size large >/dev/null
"$occtl" run hello >/dev/null
# a consumer that always fails, so that the queue has a dead letter
(cd "$here" && "$ORIN" build --quiet --target=wasm32-orbit tools/fixtures/fail.or -o "$tmp/fail.wasm")
printf '{"kind":"consumer","queue":"orders","max_attempts":1}' > "$tmp/flaky.json"
"$occtl" deploy flaky "$tmp/fail.wasm" --spec "$tmp/flaky.json" >/dev/null
"$occtl" queue send orders '{"id": 1}' >/dev/null
for _ in $(seq 1 100); do
  curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$ORBITD_PORT/v1/queues" | grep -q '"dead":1' && break
  sleep 0.2
done

echo "== console"
printf '%s' "$TOKEN" > "$tmp/api-token"
printf '%s' "$CONSOLE_TOKEN" > "$tmp/console-token"
"$here/build/dev/orbit-console" --listen "127.0.0.1:$CONSOLE_PORT" --api "$ORBIT_CLOUD_URL" \
  --api-token-file "$tmp/api-token" --console-token-file "$tmp/console-token" \
  --assets "$here/assets" --client-js "$here/.orin/packages/orin-ui/client/orin-ui.js" >"$tmp/console.log" 2>&1 &
pids+=($!)
c="http://127.0.0.1:$CONSOLE_PORT"
for _ in $(seq 1 50); do curl -fs "$c/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
if ! kill -0 "${pids[1]}" 2>/dev/null; then echo "FAIL: the console did not start"; cat "$tmp/console.log"; exit 1; fi

echo "== sign-in"
check "pages redirect to /login without a session" bash -c "curl -s -o /dev/null -w '%{http_code} %{redirect_url}' $c/ | grep -q '303 .*/login'"
check "a wrong token is refused" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -d token=nope $c/login)\" = 401 ]"
check "the right token signs in and sets an HttpOnly cookie" bash -c "curl -s -i -c $tmp/jar -d token=$CONSOLE_TOKEN $c/login | tr -d '\r' | grep -qi '^set-cookie: orbit_console=.*HttpOnly'"
check "a bearer token works for scripts" bash -c "curl -s -H 'Authorization: Bearer $CONSOLE_TOKEN' $c/runs | grep -q 'Recent runs'"
check "?token= is exchanged for a cookie and redirected" bash -c "curl -s -o /dev/null -w '%{http_code} %{redirect_url}' '$c/queues?token=$CONSOLE_TOKEN' | grep -q '303 .*/queues'"
get() { curl -s -b "$tmp/jar" "$c$1"; }

echo "== screens"
check "apps list shows both apps" bash -c "curl -s -b $tmp/jar $c/ | grep -q 'href=\"/apps/hello\"' && curl -s -b $tmp/jar $c/ | grep -q 'href=\"/apps/flaky\"'"
check "app page: deployments, active marker, run button, kv" bash -c "curl -s -b $tmp/jar $c/apps/hello | grep -q '2 (active)' && curl -s -b $tmp/jar $c/apps/hello | grep -q 'Run now' && curl -s -b $tmp/jar $c/apps/hello | grep -q 'cfg (2)'"
run_id="$("$occtl" runs hello | awk 'NR==2 {print $1}')"
check "run page: trace and logs" bash -c "curl -s -b $tmp/jar $c/runs/$run_id | grep -q '<td class=\"child\">execute</td>' && curl -s -b $tmp/jar $c/runs/$run_id | grep -q 'class=\"bar\"'"
check "recent runs lists the run" bash -c "curl -s -b $tmp/jar $c/runs | grep -q $run_id"
check "queues page shows the dead letter" bash -c "curl -s -b $tmp/jar $c/queues | grep -q 'Dead letters'"
check "logs page opens the stream" bash -c "curl -s -b $tmp/jar $c/logs | grep -q 'data-ou-stream=\"/ui/stream\"'"
check "an unknown app is a 404 page" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -b $tmp/jar $c/apps/nope)\" = 404 ]"

echo "== patch stream"
sid="$(get /apps/hello | sed -n 's/.*data-ou-session="\([^"]*\)".*/\1/p')"
patch="$(curl -s -b "$tmp/jar" -H "X-Orin-UI-Session: $sid" -d '{"t":"event","path":[0,1,1,0],"event":"click","value":""}' "$c/ui/events")"
check "run now answers with a numbered patch" bash -c "echo '$patch' | grep -q '^{\"t\":\"patch\",\"n\":0,' && echo '$patch' | grep -q 'succeeded, exit 0'"
lsid="$(get /logs | sed -n 's/.*data-ou-session="\([^"]*\)".*/\1/p')"
curl -s -b "$tmp/jar" -H "X-Orin-UI-Session: $lsid" -d '{"t":"event","path":[0,1,1,4],"event":"click","value":""}' "$c/ui/events" >/dev/null
"$occtl" run hello >/dev/null
check "the stream pushes a log line that arrived after Follow" bash -c "curl -s -N --max-time 3 -b $tmp/jar '$c/ui/stream?s=$lsid' | grep -q '^data: {\"t\":\"patch\",\"n\":1,'"

if command -v node >/dev/null && [ -n "${PLAYWRIGHT:-}" ]; then
  echo "== browser"
  mkdir -p "${SHOTS:-$tmp/shots}"
  if CONSOLE_URL="$c" CONSOLE_TOKEN="$CONSOLE_TOKEN" OCCTL="$occtl" node "$here/tools/browser_check.js" "${SHOTS:-$tmp/shots}"; then
    echo "ok: browser check"
  else
    echo "FAIL: browser check"; fails=$((fails + 1))
  fi
else
  echo "== browser: skipped (set PLAYWRIGHT to a playwright module and have node)"
fi

echo "== console log"
tail -n 5 "$tmp/console.log"
if [ "$fails" -ne 0 ]; then echo "$fails check(s) failed"; exit 1; fi
echo "e2e ok"
