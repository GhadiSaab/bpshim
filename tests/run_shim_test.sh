#!/usr/bin/env bash
# Runs tests/shim_test.c under LD_PRELOAD against one local dtn7-rs node (ipn:10.0).
set -u
cd "$(dirname "$0")/.."
ROOT=$PWD
DTND=$(command -v dtnd || true)
[ -z "$DTND" ] && [ -x "$ROOT/.tools/bin/dtnd" ] && DTND=$ROOT/.tools/bin/dtnd
[ -n "$DTND" ] || { echo "dtnd not found (run 'make deps')" >&2; exit 1; }
[ -e libbpshim.so ] || { echo "run 'make' first" >&2; exit 1; }

HTTP=${HTTP10:-3010}
WORK=$(mktemp -d)
trap 'kill $NODE 2>/dev/null; wait 2>/dev/null; rm -rf "$WORK"' EXIT

cc -O0 -g -Wall -I third_party/bp-socket/include -o "$WORK/shim_test" tests/shim_test.c || exit 1
"$DTND" -n ipn:10.0 -W "$WORK" -w "$HTTP" -C tcp:port=${TCP10:-4010} --disable_nd -r epidemic -D mem >"$WORK/node.log" 2>&1 &
NODE=$!
for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$HTTP/status/eids" >/dev/null 2>&1 && break; sleep 0.1; done

BP_DTN7_URL="http://127.0.0.1:$HTTP" LD_PRELOAD="$ROOT/libbpshim.so" "$WORK/shim_test"
