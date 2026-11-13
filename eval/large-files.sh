#!/usr/bin/env bash
# Large-file evaluation (brief §6) with real `nebula run` daemons over
# encrypted sessions on this machine (loopback). For each size: time from the
# receiver starting until its copy matches byte-for-byte, throughput, and the
# peak resident memory of both daemons. Then kills the receiver halfway
# through the largest file and restarts it to measure resume.
#
# Usage: eval/large-files.sh [sizes in MiB...]    (default: 1 100 1024)
# Output: Markdown on stdout.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build -q --release -p nebula-cli
N="$PWD/target/release/nebula"
SIZES=("${@:-1 100 1024}")
# shellcheck disable=SC2206
SIZES=(${SIZES[*]})
WORK=$(mktemp -d)
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

now() { python3 -c 'import time; print(time.time())'; }
rss_kib() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' || echo 0; }

# Two paired devices, A (sender) and B (receiver), syncing folder "eval".
setup() {
  rm -rf "$WORK/a" "$WORK/b" "$WORK/fa" "$WORK/fb"
  mkdir -p "$WORK/fa" "$WORK/fb"
  "$N" --home "$WORK/a" name "Sender" >/dev/null
  "$N" --home "$WORK/b" name "Receiver" >/dev/null
  "$N" --home "$WORK/a" pair --port 17979 >"$WORK/pair.log" 2>&1 &
  local pp=$!
  sleep 1
  local code
  code=$(grep "Pairing code" "$WORK/pair.log" | awk '{print $3}')
  "$N" --home "$WORK/b" pair "$code" --addr 127.0.0.1:17979 >/dev/null 2>&1
  wait "$pp" || true
  "$N" --home "$WORK/a" add "$WORK/fa" --as eval >/dev/null
  "$N" --home "$WORK/b" add "$WORK/fb" --as eval >/dev/null
}

start_a() {
  "$N" --home "$WORK/a" run --port 17911 >"$WORK/a.log" 2>&1 &
  A_PID=$!
  PIDS+=("$A_PID")
  # Wait until A has indexed the file.
  until grep -q "added" "$WORK/a.log" 2>/dev/null; do sleep 0.2; done
}

start_b() {
  "$N" --home "$WORK/b" run --port 17912 --connect 127.0.0.1:17911 >>"$WORK/b.log" 2>&1 &
  B_PID=$!
  PIDS+=("$B_PID")
}

# Waits until B reports the file installed, then checks it byte-for-byte.
# Tracks peak memory. Sets ELAPSED, PEAK_A, PEAK_B.
wait_synced() {
  local start=$1 peak_a=0 peak_b=0 r
  # Polling the log instead of comparing 1 GB files keeps the measurement
  # from competing with the transfer for disk bandwidth.
  until grep -q "data.bin: updated from the other device" "$WORK/b.log" 2>/dev/null; do
    r=$(rss_kib "$A_PID"); ((r > peak_a)) && peak_a=$r
    r=$(rss_kib "$B_PID"); ((r > peak_b)) && peak_b=$r
    sleep 0.1
  done
  ELAPSED=$(python3 -c "print($(now) - $start)")
  cmp -s "$WORK/fa/data.bin" "$WORK/fb/data.bin" || { echo "copies differ!" >&2; exit 1; }
  PEAK_A=$peak_a
  PEAK_B=$peak_b
}

stop() { kill "$A_PID" "$B_PID" 2>/dev/null || true; wait "$A_PID" "$B_PID" 2>/dev/null || true; }

echo "# Nebula evaluation: large files over real encrypted sessions"
echo
echo "Two \`nebula run\` daemons on one machine ($(uname -sm)), loopback TCP, Noise-encrypted."
echo "Time runs from starting the receiver until its copy is byte-identical (\`cmp\`), so it"
echo "includes connecting, the handshake, the index exchange and the final hash check."
echo
echo "| File size | Time | Throughput | Peak memory, sender | Peak memory, receiver | Copy identical |"
echo "|---|---|---|---|---|---|"
for mib in "${SIZES[@]}"; do
  setup
  head -c $((mib * 1024 * 1024)) /dev/urandom >"$WORK/fa/data.bin"
  : >"$WORK/b.log"
  start_a
  t0=$(now)
  start_b
  wait_synced "$t0"
  stop
  python3 - "$mib" "$ELAPSED" "$PEAK_A" "$PEAK_B" <<'PY'
import sys
mib, secs, a, b = int(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
size = f"{mib} MiB" if mib < 1024 else f"{mib/1024:g} GiB"
print(f"| {size} | {secs:.2f} s | {mib/secs:.1f} MiB/s | {a/1024:.1f} MiB | {b/1024:.1f} MiB | yes |")
PY
done

# Resume: kill the receiver at about half way through the largest file.
mib=${SIZES[${#SIZES[@]}-1]}
total=$((mib * 1024 * 1024))
setup
head -c "$total" /dev/urandom >"$WORK/fa/data.bin"
: >"$WORK/b.log"
start_a
start_b
partial=0
while ((partial < total / 2)); do
  partial=$(find "$WORK/fb" -name '.nebula-tmp-*' -exec stat -f %z {} \; 2>/dev/null | head -1)
  partial=${partial:-0}
  sleep 0.05
done
kill -9 "$B_PID"
wait "$B_PID" 2>/dev/null || true
kept=$(find "$WORK/fb" -name '.nebula-tmp-*' -exec stat -f %z {} \; | head -1)
t0=$(now)
start_b
wait_synced "$t0"
resumed=$(grep -o "resuming; [0-9]* of [0-9]* chunks" "$WORK/b.log" | tail -1 || true)
stop
echo
echo "## Resume after the receiver is killed (kill -9) mid-transfer"
echo
python3 - "$mib" "$kept" "$ELAPSED" "$resumed" <<'PY'
import sys
mib, kept, secs, line = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
print(f"- File: {mib} MiB. Partial download on disk when killed: {kept/2**20:.1f} MiB ({kept*100/(mib*2**20):.0f} %).")
print(f"- After restarting the receiver, the copy was complete and byte-identical in {secs:.2f} s.")
print(f"- Receiver log: \"{line}\" (only the remaining chunks were requested).")
PY
