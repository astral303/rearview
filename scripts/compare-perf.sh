#!/usr/bin/env bash
# Compare the search stage timings of two rearview binaries over the sessions
# on this machine, each binary with its own scratch cache.
#
# Usage:
#   scripts/compare-perf.sh --base <binary> --base-cache <dir> \
#     --candidate <binary> --candidate-cache <dir> [--rounds <n>] <query>...
#
# For each query, each round runs `agent search <query> --mode lexical
# --perf-stats` and `--debug-search <query>` under both binaries. The table
# shows, per query and stage, each binary's median over the rounds and the
# speedup (median, base / candidate), then the work counts each binary
# reported. A stage or count missing from either binary in any round is
# named instead of compared.
set -euo pipefail

usage="usage: scripts/compare-perf.sh --base <binary> --base-cache <dir> --candidate <binary> --candidate-cache <dir> [--rounds <n>] <query>..."

fail() {
  echo "$1" >&2
  exit 2
}

require_value() {
  [ "$#" -ge 2 ] && [ -n "$2" ] && [[ "$2" != --* ]] || fail "$1 needs a value; $usage"
}

base=""
base_cache=""
candidate=""
candidate_cache=""
rounds=5
while [ "$#" -gt 0 ]; do
  case "$1" in
    --base) require_value "$@"; base="$2"; shift 2 ;;
    --base-cache) require_value "$@"; base_cache="$2"; shift 2 ;;
    --candidate) require_value "$@"; candidate="$2"; shift 2 ;;
    --candidate-cache) require_value "$@"; candidate_cache="$2"; shift 2 ;;
    --rounds) require_value "$@"; rounds="$2"; shift 2 ;;
    --) shift; break ;;
    -*) fail "unknown option $1; $usage" ;;
    *) break ;;
  esac
done
queries=("$@")

[ -n "$base" ] && [ -n "$candidate" ] || fail "$usage"
[ "${#queries[@]}" -gt 0 ] || fail "no query given; $usage"
# Without a cache directory the binary would read and rewrite the user's
# real cache.
[ -n "$base_cache" ] && [ -n "$candidate_cache" ] ||
  fail "--base-cache and --candidate-cache are both required: each binary gets its own scratch cache"
[[ "$rounds" =~ ^[1-9][0-9]*$ ]] || fail "--rounds must be a positive whole number, not $rounds"
for binary in "$base" "$candidate"; do
  [ -x "$binary" ] || fail "$binary is not an executable file"
done
# The records carry one query per line.
for query in "${queries[@]}"; do
  [[ "$query" != *$'\n'* ]] || fail "a query cannot hold a newline"
done
mkdir -p "$base_cache" "$candidate_cache"
[ ! "$base_cache" -ef "$candidate_cache" ] ||
  fail "--base-cache and --candidate-cache must be different directories: two versions can write different cache formats"
if [ "$rounds" -lt 5 ]; then
  echo "warning: $rounds rounds; with fewer than five, one disturbed run can move a median" >&2
fi

records="$(mktemp)"
trap 'rm -f "$records"' EXIT
run=0

# Runs one command under `side`'s binary and cache, and appends a line to
# $records for each timing and work count it reports on stderr:
# query index, side, run number, `time` or `count`, the record's name, value.
measure() {
  local query_index="$1" side="$2" command="$3"
  shift 3
  local binary="$base" cache="$base_cache" stderr
  if [ "$side" = candidate ]; then
    binary="$candidate"
    cache="$candidate_cache"
  fi
  if ! stderr="$(REARVIEW_CACHE_DIR="$cache" "$binary" "$@" 2>&1 > /dev/null)"; then
    printf '%s binary failed: %s %s\n%s\n' "$side" "$binary" "$*" "$stderr" >&2
    exit 1
  fi
  run=$((run + 1))
  printf '%s\n' "$stderr" | tr -d '\r' | awk \
    -v prefix="$query_index\t$side\t$run\t" -v command="$command" '
    function emit(kind, name, value) { print prefix kind "\t" command " " name "\t" value }
    $1 != "perf" { next }
    $2 ~ /^stage=/ && $3 ~ /^ms=[0-9]+$/ { emit("time", substr($2, 7), substr($3, 4)) }
    $2 == "total" && $3 ~ /^ms=[0-9]+$/ { emit("time", "total", substr($3, 4)) }
    $2 == "evidence" && $3 ~ /^rows=[0-9]+$/ { emit("count", "evidence rows", substr($3, 6)) }
    $2 == "reads" {
      for (i = 3; i <= NF; i++) {
        if ($i ~ /^sessions=[0-9]+$/) emit("count", "sessions read", substr($i, 10))
        if ($i ~ /^transcript-bytes=[0-9]+$/) emit("count", "transcript-bytes", substr($i, 18))
      }
    }
  ' >> "$records"
}

run_queries() {
  local side="$1" index
  for index in "${!queries[@]}"; do
    measure "$index" "$side" agent-search agent search "${queries[$index]}" --mode lexical --perf-stats
    measure "$index" "$side" debug-search --debug-search "${queries[$index]}"
  done
}

echo "warm-up: filling or updating each binary's cache" >&2
run_queries base
run_queries candidate
: > "$records"

for round in $(seq 1 "$rounds"); do
  echo "round $round of $rounds" >&2
  # Alternating the order spreads a warmer OS file cache over both binaries.
  if [ $((round % 2)) -eq 1 ]; then
    run_queries base
    run_queries candidate
  else
    run_queries candidate
    run_queries base
  fi
done

printf '%s\n' "${queries[@]}" | awk -F '\t' -v rounds="$rounds" '
  FNR == NR { query_text[NR - 1] = $0; query_count = NR; next }
  {
    query = $1; side = $2; run = $3; name = $5
    if (!((query, name) in kind)) {
      kind[query, name] = $4
      name_count[query]++
      name_at[query, name_count[query]] = name
    }
    # A stage that runs more than once in one run counts once, summed.
    if (!((query, name, side, run) in sum)) slot[query, name, side, run] = ++count[query, name, side]
    sum[query, name, side, run] += $6
    value[query, name, side, slot[query, name, side, run]] = sum[query, name, side, run]
  }
  function median(query, name, side,    n, i, j, sorted, swap) {
    n = count[query, name, side]
    for (i = 1; i <= n; i++) sorted[i] = value[query, name, side, i]
    for (i = 2; i <= n; i++) {
      swap = sorted[i]
      for (j = i - 1; j >= 1 && sorted[j] > swap; j--) sorted[j + 1] = sorted[j]
      sorted[j + 1] = swap
    }
    if (n % 2) return sorted[(n + 1) / 2]
    return (sorted[n / 2] + sorted[n / 2 + 1]) / 2
  }
  function shortfall(query, name, side,    n) {
    n = count[query, name, side] + 0
    if (n == 0) return side " did not print it"
    if (n < rounds) return side " printed it in " n " of " rounds " rounds"
    return ""
  }
  # The awk macOS ships rejects a conditional expression among a printf or
  # sprintf argument list, so each one is assigned to a variable first.
  function compared(query, name,    base_missing, candidate_missing, missing) {
    base_missing = shortfall(query, name, "base")
    candidate_missing = shortfall(query, name, "candidate")
    if (base_missing == "" && candidate_missing == "") return 1
    missing = base_missing candidate_missing
    if (base_missing != "" && candidate_missing != "") missing = base_missing "; " candidate_missing
    notes = notes sprintf("    %s: %s\n", name, missing)
    return 0
  }
  END {
    printf "Medians over %d rounds, in milliseconds; speedup = base / candidate\n", rounds
    for (query = 0; query < query_count; query++) {
      notes = ""
      printf "\nquery: %s\n", query_text[query]
      printf "  %-36s %10s %10s %10s\n", "stage", "base", "candidate", "speedup"
      for (i = 1; i <= name_count[query]; i++) {
        name = name_at[query, i]
        if (kind[query, name] != "time" || !compared(query, name)) continue
        base = median(query, name, "base")
        candidate = median(query, name, "candidate")
        speedup = "-"
        if (candidate > 0) speedup = sprintf("%.2f", base / candidate)
        printf "  %-36s %10s %10s %10s\n", name, base, candidate, speedup
      }
      counts_header = sprintf("  %-36s %10s %10s\n", "work count", "base", "candidate")
      for (i = 1; i <= name_count[query]; i++) {
        name = name_at[query, i]
        if (kind[query, name] != "count" || !compared(query, name)) continue
        base = median(query, name, "base")
        candidate = median(query, name, "candidate")
        differs = ""
        if (base != candidate) differs = "  differs"
        printf "%s  %-36s %10s %10s%s\n", counts_header, name, base, candidate, differs
        counts_header = ""
      }
      if (notes != "") printf "  not compared:\n%s", notes
    }
  }
' - "$records"
