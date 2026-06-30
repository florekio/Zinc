#!/bin/bash
# Phase 0 perf harness: per-benchmark wall time, dispatched ops, ops/sec,
# and the Zinc-vs-Node gap. Lighthouse (decode) also dumps an opcode histogram.
#   bench/profile.sh            # run the perf-milestone benchmark set
#   bench/profile.sh hist       # also print the decode opcode histogram
ZINC="$(dirname "$0")/../target/release/zinc"
BENCH_DIR="$(dirname "$0")"
[ -x "$ZINC" ] || { echo "build first: cargo build --release"; exit 1; }

printf "%-12s %9s %14s %11s %9s %8s\n" "bench" "zinc(s)" "ops" "Mops/s" "node(s)" "gap"
echo "──────────────────────────────────────────────────────────────────────"
for f in decode numeric strings props; do
  file="$BENCH_DIR/$f.js"
  [ -f "$file" ] || continue
  # Zinc timing + op count (ZINC_TIME enables the counter)
  out=$(ZINC_TIME=1 "$ZINC" "$file" 2>&1 >/dev/null)
  wall=$(echo "$out" | sed -nE 's/.*: ([0-9.]+)s wall.*/\1/p')
  ops=$(echo "$out"  | sed -nE 's/^ *([0-9]+) ops dispatched.*/\1/p')
  mops=$(echo "$out" | sed -nE 's/.*, ([0-9.]+)M ops\/sec/\1/p')
  # Node baseline
  if command -v node >/dev/null 2>&1; then
    ns=$(python3 -c "import time,subprocess;t=time.time();subprocess.run(['node','$file'],capture_output=True);print(f'{time.time()-t:.3f}')")
    gap=$(python3 -c "z=$wall; n=$ns; print(f'{z/n:.0f}x' if n>0.0005 else 'NA')" 2>/dev/null)
  else ns="NA"; gap="NA"; fi
  printf "%-12s %9s %14s %11s %9s %8s\n" "$f" "${wall:-?}" "${ops:-?}" "${mops:-?}" "$ns" "$gap"
done
echo ""
if [ "$1" = "hist" ]; then
  echo "=== decode.js opcode histogram (the lighthouse hot path) ==="
  ZINC_OPCODE_HIST=1 "$ZINC" "$BENCH_DIR/decode.js" 2>&1 >/dev/null | grep -E "===|%" | head -25
fi
