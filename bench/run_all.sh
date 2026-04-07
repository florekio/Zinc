#!/bin/bash
# Zinc vs Node.js Benchmark Suite
set -e

ZINC="$(dirname "$0")/../target/release/zinc"
NODE="node"
BENCH_DIR="$(dirname "$0")"

if [ ! -f "$ZINC" ]; then
    echo "Error: Release build not found. Run: cargo build --release"
    exit 1
fi

# Colors
BOLD="\033[1m"
DIM="\033[2m"
RESET="\033[0m"
CYAN="\033[36m"
YELLOW="\033[33m"
GREEN="\033[32m"

echo ""
echo -e "${BOLD}=== Zinc vs Node.js Benchmark ===${RESET}"
echo ""
echo -e "${DIM}Zinc:  $($ZINC --version 2>/dev/null || echo "Zinc JS Engine")${RESET}"
echo -e "${DIM}Node:  $(node --version)${RESET}"
echo ""

printf "${BOLD}%-22s %10s %10s %10s${RESET}\n" "Benchmark" "Zinc" "Node" "Ratio"
echo "────────────────────────────────────────────────────────"

run_bench() {
    local name="$1"
    local file="$2"

    # Run Zinc (capture time)
    local zinc_start=$(python3 -c "import time; print(time.time())")
    local zinc_result=$($ZINC "$file" 2>/dev/null)
    local zinc_end=$(python3 -c "import time; print(time.time())")
    local zinc_time=$(python3 -c "print(f'{$zinc_end - $zinc_start:.3f}')")

    # Run Node (capture time)
    local node_start=$(python3 -c "import time; print(time.time())")
    local node_result=$($NODE "$file" 2>/dev/null)
    local node_end=$(python3 -c "import time; print(time.time())")
    local node_time=$(python3 -c "print(f'{$node_end - $node_start:.3f}')")

    # Check correctness
    local status=""
    if [ "$zinc_result" = "$node_result" ]; then
        status="${GREEN}✓${RESET}"
    else
        status="${YELLOW}✗${RESET} (zinc=$zinc_result node=$node_result)"
    fi

    # Calculate ratio
    local ratio=$(python3 -c "
z = $zinc_time
n = $node_time
if n > 0.0001:
    print(f'{z/n:.1f}x')
else:
    print('N/A')
")

    printf "%-22s %9ss %9ss %9s  %b\n" "$name" "$zinc_time" "$node_time" "$ratio" "$status"
}

run_bench "fibonacci(35)" "$BENCH_DIR/fib.js"
run_bench "loop_sum(1M)" "$BENCH_DIR/loop_sum.js"
run_bench "string_concat(10K)" "$BENCH_DIR/string_concat.js"
run_bench "closure_counter(100K)" "$BENCH_DIR/closure_counter.js"
run_bench "object_create(100K)" "$BENCH_DIR/object_create.js"
run_bench "sieve(10K)" "$BENCH_DIR/sieve.js"

echo ""
echo -e "${DIM}Note: Zinc is a bytecode interpreter. Node.js (V8) uses a JIT compiler.${RESET}"
echo -e "${DIM}Ratios > 1x mean Node is faster. Lower is better for Zinc.${RESET}"
echo ""
