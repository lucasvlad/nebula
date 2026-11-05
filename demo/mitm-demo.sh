#!/usr/bin/env bash
# Live demo: an eavesdropper and a middleman between two real Nebula devices.
# Usage: demo/mitm-demo.sh [--no-pause]
set -euo pipefail

cd "$(dirname "$0")/.."
NO_PAUSE=0
[[ "${1:-}" == "--no-pause" ]] && NO_PAUSE=1

PORT_PAIR=17979
PORT_RUN=17978
PORT_SNOOP=17900
PORT_IMP=17901
MESSAGE="Meet at the library at 7. My locker code is 4821."

bold() {
  if [[ -n "${NO_COLOR:-}" ]]; then printf '\n== %s ==\n' "$*"
  else printf '\n\033[1m== %s ==\033[0m\n' "$*"; fi
}
pause() {
  [[ $NO_PAUSE -eq 1 ]] && return 0
  read -r -p $'\nPress Enter to continue...' _
}

TMP="$(mktemp -d)"
PIDS=()
cleanup() {
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

bold "Building (release)"
cargo build --release -q -p nebula-cli -p nebula-tests
NEBULA=target/release/nebula
SNOOP=target/release/mitm-demo
A=(env NEBULA_HOME="$TMP/alice" "$NEBULA")
B=(env NEBULA_HOME="$TMP/bob" "$NEBULA")

bold "Setting up two devices and pairing them"
"${A[@]}" name "Alice's laptop" >/dev/null
"${B[@]}" name "Bob's desktop" >/dev/null
"${A[@]}" pair --port $PORT_PAIR >"$TMP/alice-pair.log" 2>&1 &
PAIR_PID=$!
PIDS+=("$PAIR_PID")
CODE=""
for _ in $(seq 1 50); do
  CODE="$(grep 'Pairing code:' "$TMP/alice-pair.log" 2>/dev/null | awk '{print $3}' || true)"
  [[ -n "$CODE" ]] && break
  sleep 0.2
done
[[ -n "$CODE" ]] || { echo "could not read pairing code"; cat "$TMP/alice-pair.log"; exit 1; }
"${B[@]}" pair "$CODE" --addr 127.0.0.1:$PORT_PAIR
wait "$PAIR_PID" 2>/dev/null || true
echo "Paired: Alice's laptop and Bob's desktop."

"${A[@]}" run --port $PORT_RUN >"$TMP/alice-run.log" 2>&1 &
PIDS+=($!)
sleep 1

run_snoop_case() {
  local title="$1"
  shift
  bold "$title"
  "$SNOOP" snoop --listen 127.0.0.1:$PORT_SNOOP --to 127.0.0.1:$PORT_RUN --look-for locker "$@" \
    >"$TMP/snoop.log" 2>&1 &
  local sp=$!
  PIDS+=("$sp")
  sleep 1
  echo "Bob's desktop pings Alice's laptop through the snoop, with a secret message:"
  "${B[@]}" ping --addr 127.0.0.1:$PORT_SNOOP -c 1 --message "$MESSAGE" || echo "(Bob's ping failed, as expected when tampering)"
  sleep 1
  echo
  echo "--- What the snoop saw ---"
  cat "$TMP/snoop.log"
  echo
  echo "--- Alice's log (last lines) ---"
  tail -n 8 "$TMP/alice-run.log"
  kill "$sp" 2>/dev/null || true
  wait "$sp" 2>/dev/null || true
}

pause
run_snoop_case "Step 1: an eavesdropper between two real devices"
pause
run_snoop_case "Step 2: the same, but the eavesdropper flips one bit" --tamper-at 150
pause

bold "Step 3: an impersonator (Mallory) in the middle"
"$SNOOP" impersonate --listen 127.0.0.1:$PORT_IMP --to 127.0.0.1:$PORT_RUN >"$TMP/imp.log" 2>&1 &
ip=$!
PIDS+=("$ip")
sleep 1
"${B[@]}" ping --addr 127.0.0.1:$PORT_IMP -c 1 --message "$MESSAGE" || echo "(Bob's ping failed, as expected)"
wait "$ip" 2>/dev/null || true
cat "$TMP/imp.log"
bold "Done"
