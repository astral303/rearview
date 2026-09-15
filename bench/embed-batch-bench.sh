#!/usr/bin/env bash
# Embedding batch-size benchmark, macOS and Linux.
#
# Workload: the real semantic catch-up (`--generate-semantic-cache`) against a
# copy of ~/.cache/rearview/semantic, so every batch size embeds the same
# pending chunks. One control run with nothing to embed measures corpus load,
# model init and baseline memory; subtract it from the others.
#
# Prerequisites: this branch built with `cargo build --release`, a semantic
# cache with chunks still to embed (open the TUI in semantic mode and quit
# before it finishes, or add sessions).
#
# Usage: bench/embed-batch-bench.sh [batch sizes...]   (default: 8 16 32 64 128 256)
# Output: one row per run, and results.csv under ~/.cache/rearview-embed-bench/logs.
set -euo pipefail

exe="$(cd "$(dirname "$0")/.." && pwd)/target/release/rearview"
real="$HOME/.cache/rearview/semantic"
bench="$HOME/.cache/rearview-embed-bench"
pending="$bench/pending"   # the real cache as of now: chunks still to embed
complete="$bench/complete" # after one full catch-up: nothing to embed
run="$bench/run"
logs="$bench/logs"
if [ $# -eq 0 ]; then
    sizes=(8 16 32 64 128 256)
else
    sizes=("$@")
fi

if [ "$(uname)" = Darwin ]; then
    time_cmd=(/usr/bin/time -l)
    rss_key="maximum resident set size"
    rss_div=1048576
else
    time_cmd=(/usr/bin/time -v)
    rss_key="Maximum resident set size"
    rss_div=1024
fi

[ -d "$pending" ] || { mkdir -p "$bench"; cp -R "$real" "$pending"; }
mkdir -p "$logs"

# invoke_run <label> <source cache dir> <batch>
invoke_run() {
    local label=$1 source=$2 batch=$3
    rm -rf "$run"
    cp -R "$source" "$run"
    local err="$logs/$label.stderr.txt" timing="$logs/$label.time.txt"
    # `time` reports on its direct child, so the binary is exec'd from a shell
    # that only redirects the binary's own stderr; time's report goes to $timing.
    REARVIEW_BENCH_SEMANTIC_DIR="$run" REARVIEW_BENCH_BATCH="$batch" \
        "${time_cmd[@]}" sh -c 'exec "$0" --generate-semantic-cache 2>"$1"' "$exe" "$err" \
        >"$logs/$label.stdout.txt" 2>"$timing" || true
    local embedded seconds peak_mb
    # A run with nothing to embed prints no `embedded N/M` line.
    embedded=$(awk 'match($0, /embedded [0-9]+\/[0-9]+/) { n = substr($0, RSTART, RLENGTH); sub(/.*\//, "", n) } END { print n + 0 }' "$err")
    seconds=$(awk '/real/ { print $1; exit }' "$timing")
    peak_mb=$(awk -v key="$rss_key" -v div="$rss_div" 'index($0, key) { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+$/) { printf "%d", $i / div; exit } }' "$timing")
    printf '%-18s %6s %9s %9s %8s\n' "$label" "$batch" "$embedded" "$seconds" "$peak_mb"
    echo "$label,$batch,$embedded,$seconds,$peak_mb" >>"$logs/results.csv"
}

: >"$logs/results.csv"
printf '%-18s %6s %9s %9s %8s\n' run batch embedded seconds peak_MB
# The first full catch-up at the default batch; its output is the zero-miss control.
invoke_run b32-first "$pending" 32
rm -rf "$complete"
cp -R "$run" "$complete"
invoke_run control-no-misses "$complete" 32
for b in "${sizes[@]}"; do
    invoke_run "b$b" "$pending" "$b"
done
rm -rf "$run"
