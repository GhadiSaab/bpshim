#!/usr/bin/env bash
# Two dtn7-rs nodes (ipn:10 and ipn:20) on localhost. The repo's unmodified
# receiver and sender run under LD_PRELOAD=./libbpshim.so: no kernel module, no root.
set -u
cd "$(dirname "$0")"
ROOT=$PWD

HTTP10=${HTTP10:-3010}; TCP10=${TCP10:-4010}
HTTP20=${HTTP20:-3020}; TCP20=${TCP20:-4020}

say() { printf '\033[1m[demo]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[demo] %s\033[0m\n' "$*" >&2; exit 1; }

DTND=$(command -v dtnd || true)
[ -z "$DTND" ] && [ -x "$ROOT/.tools/bin/dtnd" ] && DTND=$ROOT/.tools/bin/dtnd
[ -n "$DTND" ] || die "dtnd not found. Run 'make deps' (installs dtn7-rs into ./.tools) or put dtnd on PATH."
for f in libbpshim.so sender receiver; do
  [ -e "$f" ] || die "$f missing. Run 'make' first."
done

WORK=$(mktemp -d)
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

say "running as uid $(id -u) (no root needed)"

start_node() { # <node number> <http port> <tcp port>
  mkdir -p "$WORK/n$1"
  "$DTND" -n "ipn:$1.0" -W "$WORK/n$1" -w "$2" -C "tcp:port=$3" \
    --disable_nd -r epidemic -D mem >"$WORK/n$1.log" 2>&1 &
  PIDS+=($!)
}

wait_http() { # <port>
  for _ in $(seq 1 100); do
    curl -sf "http://127.0.0.1:$1/status/eids" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

say "starting dtn7-rs nodes ipn:10 (http :$HTTP10, tcp :$TCP10) and ipn:20 (http :$HTTP20, tcp :$TCP20)"
start_node 10 "$HTTP10" "$TCP10"
start_node 20 "$HTTP20" "$TCP20"
wait_http "$HTTP10" || { cat "$WORK/n10.log"; die "node ipn:10 did not come up"; }
wait_http "$HTTP20" || { cat "$WORK/n20.log"; die "node ipn:20 did not come up"; }

say "linking the nodes to each other"
curl -sf "http://127.0.0.1:$HTTP10/peers/add?p=tcp://127.0.0.1:$TCP20/ipn:20.0&p_t=STATIC" >/dev/null || die "peer add failed (10 -> 20)"
curl -sf "http://127.0.0.1:$HTTP20/peers/add?p=tcp://127.0.0.1:$TCP10/ipn:10.0&p_t=STATIC" >/dev/null || die "peer add failed (20 -> 10)"

# The receiver prints to a file, which would be block-buffered; stdbuf makes it line-buffered
# (it composes with our LD_PRELOAD). Without stdbuf the output simply appears at exit.
LINEBUF=()
command -v stdbuf >/dev/null && LINEBUF=(stdbuf -oL)

say "starting receiver on node ipn:20 (bound to ipn:20.1)"
BP_DTN7_URL="http://127.0.0.1:$HTTP20" LD_PRELOAD="$ROOT/libbpshim.so" \
  "${LINEBUF[@]}" ./receiver 20 1 >"$WORK/receiver.log" 2>&1 &
RECV=$!
PIDS+=($RECV)
for _ in $(seq 1 50); do
  grep -q "Listening for incoming" "$WORK/receiver.log" 2>/dev/null && break
  kill -0 "$RECV" 2>/dev/null || break
  sleep 0.1
done
kill -0 "$RECV" 2>/dev/null || { cat "$WORK/receiver.log"; die "receiver exited early"; }

say "sending from node ipn:10 (sender is bound to ipn:10.2) to ipn:20.1"
BP_DTN7_URL="http://127.0.0.1:$HTTP10" LD_PRELOAD="$ROOT/libbpshim.so" ./sender 20 1 | tee "$WORK/sender.log"
# sender.c ends with "return ret" where ret is sendto()'s byte count, so its exit code is 7
# on success; judge it by its output instead.
SENT=0; grep -q "Message sent successfully" "$WORK/sender.log" && SENT=1

GOT=0
for _ in $(seq 1 100); do
  grep -q "Received message" "$WORK/receiver.log" && { GOT=1; break; }
  sleep 0.1
done

kill -INT "$RECV" 2>/dev/null
wait "$RECV" 2>/dev/null

echo
say "receiver output:"
sed 's/^/    /' "$WORK/receiver.log"
echo

if [ "$SENT" -eq 1 ] && [ "$GOT" -eq 1 ] \
   && grep -q "Received message (7 bytes): Hello!" "$WORK/receiver.log" \
   && grep -q "Bundle sent by ipn:10.2" "$WORK/receiver.log"; then
  say "OK: the unmodified receiver got the bundle from ipn:10.2"
  exit 0
fi
say "FAILED (sender ok: $SENT, receiver got message: $GOT). Node logs:"
tail -n 15 "$WORK/n10.log" "$WORK/n20.log"
exit 1
