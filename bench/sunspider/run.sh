#!/bin/bash
# SunSpider benchmark: Zinc vs Node.js
ZINC="$(dirname "$0")/../../target/release/zinc"
DIR="$(dirname "$0")"

BOLD="\033[1m"
DIM="\033[2m"
GREEN="\033[32m"
RESET="\033[0m"

echo ""
echo -e "${BOLD}=== SunSpider Benchmark: Zinc vs Node.js ===${RESET}"
echo ""
printf "${BOLD}%-30s %10s %10s %10s${RESET}\n" "Test" "Zinc" "Node" "Ratio"
echo "──────────────────────────────────────────────────────────"

run_bench() {
    local name="$1"
    local file="$2"

    local zinc_start=$(python3 -c "import time; print(time.time())")
    local zinc_result=$($ZINC "$file" 2>/dev/null)
    local zinc_end=$(python3 -c "import time; print(time.time())")
    local zinc_ms=$(python3 -c "print(f'{($zinc_end - $zinc_start)*1000:.0f}')")

    local node_start=$(python3 -c "import time; print(time.time())")
    local node_result=$(node "$file" 2>/dev/null)
    local node_end=$(python3 -c "import time; print(time.time())")
    local node_ms=$(python3 -c "print(f'{($node_end - $node_start)*1000:.0f}')")

    local status=""
    if [ "$zinc_result" = "$node_result" ]; then
        status="${GREEN}✓${RESET}"
    else
        status="✗"
    fi

    local ratio=$(python3 -c "
z = $zinc_ms
n = $node_ms
if n > 0:
    print(f'{z/n:.1f}x')
else:
    print('N/A')
")

    printf "%-30s %8sms %8sms %9s  %b\n" "$name" "$zinc_ms" "$node_ms" "$ratio" "$status"
}

for f in "$DIR"/access-nbody.js "$DIR"/access-binary-trees.js "$DIR"/access-fannkuch.js "$DIR"/access-nsieve.js "$DIR"/bitops-3bit-bits-in-byte.js "$DIR"/bitops-bitwise-and.js "$DIR"/bitops-nsieve-bits.js "$DIR"/controlflow-recursive.js "$DIR"/math-cordic.js "$DIR"/math-partial-sums.js "$DIR"/3d-cube.js "$DIR"/string-validate-input.js; do
    name=$(basename "$f" .js)
    run_bench "$name" "$f"
done

echo ""
echo -e "${DIM}Note: Times include startup. Lower ratio = better for Zinc.${RESET}"
echo ""
