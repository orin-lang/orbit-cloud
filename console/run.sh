#!/usr/bin/env bash
# Runs the console natively against an orbitd.
#
#   ORBIT_CLOUD_TOKEN=secret console/run.sh                       # orbitd on 127.0.0.1:8080, console on 127.0.0.1:8081
#   ORBIT_CLOUD_URL=http://10.0.0.5:8080 CONSOLE_TOKEN=pw console/run.sh
#
# ORBIT_CLOUD_URL   orbitd (default http://127.0.0.1:8080)         ORBIT_CLOUD_TOKEN  orbitd's bearer token
# CONSOLE_LISTEN    where the console listens (default 127.0.0.1:8081)
# CONSOLE_TOKEN     the secret that signs in to the console (default: ORBIT_CLOUD_TOKEN)
# ORIN              the orin CLI (default: orin)
#
# Orin's std cannot read environment variables, so this script hands the tokens to the console in files (not in `ps`) that are
# deleted when it exits; the console reads them once at start-up.
set -euo pipefail
cd "$(dirname "$0")"
ORIN="${ORIN:-orin}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
umask 077
args=(--api "${ORBIT_CLOUD_URL:-http://127.0.0.1:8080}" --listen "${CONSOLE_LISTEN:-127.0.0.1:8081}")
if [ -n "${ORBIT_CLOUD_TOKEN:-}" ]; then
  printf '%s' "$ORBIT_CLOUD_TOKEN" > "$tmp/api-token"; args+=(--api-token-file "$tmp/api-token")
fi
if [ -n "${CONSOLE_TOKEN:-}" ]; then
  printf '%s' "$CONSOLE_TOKEN" > "$tmp/console-token"; args+=(--console-token-file "$tmp/console-token")
fi
"$ORIN" fetch >/dev/null
"$ORIN" build --quiet
./build/dev/orbit-console "${args[@]}" "$@"
