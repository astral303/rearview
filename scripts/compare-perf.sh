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
# speedup (median, base / candidate), then each binary's median CPU time and
# the work counts each binary reported. The CPU time ratio shows `-` when
# either median is under 10 ms (156 ms on Windows). A stage or count missing
# from either binary in any round is named instead of compared.
#
# A table per query then shows every run:
# - wall and cpu: the binary's wall time and CPU time, from its start to its
#   exit. On Windows, cpu moves in 15.6 ms clock ticks.
# - others: all cores' busy time, sampled just before and after the run,
#   minus the binary's CPU time and, on Windows, the helper's. Busy time
#   includes interrupt and system work the kernel doesn't attribute to a
#   process. It is counted in steps, 10 ms on Linux and a 15.6 ms clock tick
#   on Windows; the error grows with the binary's CPU time and threads, and
#   can be positive or negative.
# - disk MB: the machine's disk use between samples taken just before and
#   after the run, the bytes every process read from and wrote to disk, the
#   binary's reads included. A run can also show writes any process made
#   before it: the OS writes cached data to disk later. A run shows `-` when a
#   sample couldn't be read, a change in the set of disks was detected, or
#   their total decreased.
# Early rounds' disk MB can include the warm-up's cache writes on macOS and
# Windows: macOS's `sync` can return before the writes reach the disk, and the
# script doesn't flush Windows' cache.
# Figures not recorded on this machine show `-` and are named under the title.
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
# The records, and the Windows helper's requests, carry one query per line.
for query in "${queries[@]}"; do
  [[ "$query" != *$'\n'* && "$query" != *$'\r'* ]] || fail "a query cannot hold a newline or a carriage return"
done
mkdir -p "$base_cache" "$candidate_cache"
[ ! "$base_cache" -ef "$candidate_cache" ] ||
  fail "--base-cache and --candidate-cache must be different directories: two versions can write different cache formats"
if [ "$rounds" -lt 5 ]; then
  echo "warning: $rounds rounds; with fewer than five, one disturbed run can move a median" >&2
fi

records="$(mktemp)"
run_stderr_file="$(mktemp)"
run_time_file="$(mktemp)"
windows_helper_pid=""
# The helper waits up to 5 s for a binary it ends before it exits itself.
windows_helper_exit_wait_seconds=10
stop_windows_helper() {
  local step
  [ -n "$windows_helper_pid" ] || return 0
  eval "exec $windows_helper_in>&-"
  for step in $(seq 1 $((windows_helper_exit_wait_seconds * 10))); do
    kill -0 "$windows_helper_pid" 2> /dev/null || break
    sleep 0.1
  done
  kill "$windows_helper_pid" 2> /dev/null || true
  wait "$windows_helper_pid" 2> /dev/null || true
  windows_helper_pid=""
}
clean_up() {
  stop_windows_helper
  rm -f "$records" "$run_stderr_file" "$run_time_file"
}
trap clean_up EXIT
# On Windows the helper ends a running binary when the script ends, by Ctrl+C
# or a signal. On Linux and macOS, Ctrl+C from the terminal reaches the binary
# itself, and the script acts on a TERM once the running binary exits: bash
# runs a trap only after the foreground command ends.
trap 'exit 130' INT
trap 'exit 143' TERM

# Warm-up cache writes: Linux's `sync` returns once they are on disk, macOS's
# can return before, and the script skips `sync` elsewhere.
on_windows=no
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) on_windows=yes; warm_up_sync=skipped ;;
  Linux) warm_up_sync=waits ;;
  Darwin) warm_up_sync=requests ;;
  *) warm_up_sync=skipped ;;
esac
# Test-only: a directory that stands in for / when reading proc/stat and
# sys/block. Empty on a real run.
system_root="${COMPARE_PERF_SYSTEM_ROOT:-}"
proc_stat="$system_root/proc/stat"
sys_block="$system_root/sys/block"
# sysfs counts 512-byte sectors, whatever the disk's block size.
sysfs_sector_bytes=512

if [ "$on_windows" = yes ]; then
  coproc powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass \
    -File "$(cygpath -w "$(dirname "$0")/compare-perf-windows.ps1")"
  windows_helper_pid="$COPROC_PID"
  windows_helper_in="${COPROC[1]}"
  windows_helper_out="${COPROC[0]}"
fi

# Sets variable $1 to the Windows helper's reply to the request in the
# further arguments, one line each. Ends the script with exit code 1 when the
# helper stops before replying.
ask_windows_helper() {
  local reply_variable="$1" helper_reply
  shift
  printf '%s\n' "$@" >&"$windows_helper_in"
  if ! IFS= read -r helper_reply <&"$windows_helper_out"; then
    # Ctrl+C in a console stops the helper too, often before bash's INT trap runs.
    echo "the Windows helper scripts/compare-perf-windows.ps1 stopped before replying, after Ctrl+C or an error above" >&2
    exit 1
  fi
  printf -v "$reply_variable" '%s' "$helper_reply"
}

# Sets variable $1 to seconds $2, such as 1.234 or 1,234, in milliseconds.
seconds_to_ms() {
  local whole="${2%[.,]*}" fraction="${2#*[.,]}000"
  printf -v "$1" '%s' $((whole * 1000 + 10#${fraction:0:3}))
}

# A runner runs binary $2 with cache $1 and the further arguments, and sets
# run_status, run_wall_ms, run_cpu_ms and run_helper_cpu_ms. Once the run's
# figures are read, the matching collector leaves the binary's stderr in
# $run_stderr_file.
run_in_bash() {
  local cache="$1" binary="$2" TIMEFORMAT='%3R %3U %3S' wall user system user_ms system_ms
  shift 2
  run_status=0
  { time REARVIEW_CACHE_DIR="$cache" "$binary" "$@" < /dev/null > /dev/null 2> "$run_stderr_file"; } \
    2> "$run_time_file" || run_status=$?
  if [ "$run_status" -ne 0 ]; then
    # Bash reports a binary killed by a signal on the line above `time`'s.
    sed '$d' "$run_time_file" >> "$run_stderr_file"
    return
  fi
  read -r wall user system < "$run_time_file"
  seconds_to_ms run_wall_ms "$wall"
  seconds_to_ms user_ms "$user"
  seconds_to_ms system_ms "$system"
  run_cpu_ms=$((user_ms + system_ms))
  run_helper_cpu_ms=0
}

collect_stderr_in_bash() {
  :
}

# Takes Windows paths for the cache and the binary.
run_through_windows_helper() {
  local cache="$1" binary="$2" reply figures
  shift 2
  ask_windows_helper reply "run $(($# + 2))" "$cache" "$binary" "$@"
  figures=($reply)
  run_status="${figures[0]}"
  run_wall_ms="${figures[1]}"
  run_cpu_ms="${figures[2]}"
  run_helper_cpu_ms="${figures[3]}"
}

collect_stderr_through_windows_helper() {
  local reply
  ask_windows_helper reply "stderr $run_stderr_file_for_helper"
}

is_native_windows_program() {
  local magic=""
  [ "$on_windows" = yes ] || return 1
  IFS= read -r -n 2 magic < "$1" || true
  [ "$magic" = MZ ]
}

# Native Windows binaries run through the helper: bash's `time` counts the CPU
# time of a Cygwin child, such as a script, but not of a native program. The
# helper's paths are converted here: running cygpath during a run would add
# its CPU time to `others`.
base_runs_natively=no
candidate_runs_natively=no
if is_native_windows_program "$base"; then base_runs_natively=yes; fi
if is_native_windows_program "$candidate"; then candidate_runs_natively=yes; fi
[ "$base_runs_natively" = "$candidate_runs_natively" ] ||
  fail "--base and --candidate must both be native Windows programs or both be scripts: their CPU times are read differently"
if [ "$base_runs_natively" = yes ]; then
  runner=run_through_windows_helper
  collect_run_stderr=collect_stderr_through_windows_helper
  base_run_binary="$(cygpath -aw "$base")"
  base_run_cache="$(cygpath -aw "$base_cache")"
  candidate_run_binary="$(cygpath -aw "$candidate")"
  candidate_run_cache="$(cygpath -aw "$candidate_cache")"
  run_stderr_file_for_helper="$(cygpath -w "$run_stderr_file")"
else
  runner=run_in_bash
  collect_run_stderr=collect_stderr_in_bash
  base_run_binary="$base"
  base_run_cache="$base_cache"
  candidate_run_binary="$candidate"
  candidate_run_cache="$candidate_cache"
fi

# Sets variable $1 to all cores' busy time since boot, in ms, or to `-` when
# it is not recorded. Busy time is user, nice, system, irq and softirq; idle,
# I/O wait and steal (a VM waiting for its host) are left out. Git Bash on
# Windows lists only user, nice, system and idle.
read_machine_busy_ms() {
  local label user nice system idle iowait irq softirq rest
  if [ "$machine_cpu_source" = none ]; then
    printf -v "$1" '%s' -
    return
  fi
  read -r label user nice system idle iowait irq softirq rest < "$proc_stat"
  printf -v "$1" '%s' $(((user + nice + system + ${irq:-0} + ${softirq:-0}) * 1000 / clock_ticks_per_second))
}

# True when sysfs device directory $1 is a disk counted once. A device-mapper
# or RAID volume doesn't have a `device` link. An NVMe multipath head
# (nvmeXnY) is left out when its path devices (nvmeXcZnY) are listed: before
# Linux 6.3 the head doesn't count its I/O, and the path devices do.
is_counted_sysfs_disk() {
  [ -e "$1/device" ] && ! is_nvme_multipath_head "$1"
}

is_nvme_multipath_head() {
  local path_device
  [[ "${1##*/}" =~ ^nvme([0-9]+)n([0-9]+)$ ]] || return 1
  for path_device in "$sys_block/nvme${BASH_REMATCH[1]}c"*"n${BASH_REMATCH[2]}"; do
    [[ "${path_device##*/}" =~ ^nvme[0-9]+c[0-9]+n[0-9]+$ ]] && return 0
  done
  return 1
}

# The sample is `-` when a disk disappears between the listing and the read.
read_sysfs_disk_sample() {
  local total=0 disks="" device reads merged sectors_read read_ms writes write_merged sectors_written rest
  for device in "$sys_block"/*; do
    is_counted_sysfs_disk "$device" || continue
    if ! read -r reads merged sectors_read read_ms writes write_merged sectors_written rest \
      2> /dev/null < "$device/stat"; then
      printf -v "$1" '%s' -
      return
    fi
    total=$((total + (sectors_read + sectors_written) * sysfs_sector_bytes))
    disks="$disks ${device##*/}"
  done
  if [ -z "$disks" ]; then
    printf -v "$1" '%s' -
  else
    printf -v "$1" '%s' "$total$disks"
  fi
}

# IOBlockStorageDriver counts a mounted disk image as well as the disk
# holding it. The script doesn't detect a disk replaced by another during a
# run: the sample names the disks only by their count.
read_ioreg_disk_sample() {
  local sample
  sample="$(ioreg -r -c IOBlockStorageDriver -k Statistics -w 0 | awk '
    function add_matches(pattern,    line, bytes, matches) {
      line = $0
      while (match(line, pattern)) {
        bytes = substr(line, RSTART, RLENGTH)
        sub(/.*=/, "", bytes)
        total += bytes
        matches++
        line = substr(line, RSTART + RLENGTH)
      }
      return matches
    }
    {
      reads += add_matches("\"Bytes \\(Read\\)\"=[0-9]+")
      writes += add_matches("\"Bytes \\(Write\\)\"=[0-9]+")
    }
    END {
      if (reads == 0 || reads != writes) print "-"
      else printf "%.0f %d\n", total, reads
    }
  ')" || sample=-
  printf -v "$1" '%s' "$sample"
}

read_windows_disk_sample() {
  ask_windows_helper "$1" disk
}

# Sets variable $1 to the bytes every counted disk has read and written since
# boot, then the disks' names (on macOS, their count), or to `-` when the
# counts cannot be read.
read_disk_sample() {
  case "$disk_source" in
    sysfs) read_sysfs_disk_sample "$1" ;;
    windows-helper) read_windows_disk_sample "$1" ;;
    ioreg) read_ioreg_disk_sample "$1" ;;
    none) printf -v "$1" '%s' - ;;
  esac
}

# Sets variable $1 to the bytes between disk samples $2 and $3, or to `-`
# when either is missing, their disks' names or count differ, or their total
# decreased.
disk_bytes_between() {
  local before_bytes="${2%% *}" after_bytes="${3%% *}"
  if [ "$2" = - ] || [ "$3" = - ] || [ "${2#* }" != "${3#* }" ] ||
    [ "$after_bytes" -lt "$before_bytes" ]; then
    printf -v "$1" '%s' -
  else
    printf -v "$1" '%s' $((after_bytes - before_bytes))
  fi
}

# Process CPU time moves in steps of 1 ms from bash's `time` on Linux and
# macOS, and in 15.6 ms clock ticks on Windows. The CPU time ratio needs both
# medians to be at least ten steps.
cpu_step_ms=1
[ "$on_windows" = no ] || cpu_step_ms=15.6
cpu_ratio_floor_ms="$(awk -v step="$cpu_step_ms" 'BEGIN { print step * 10 }')"

machine_cpu_source=none
if [ -r "$proc_stat" ]; then
  machine_cpu_source=proc-stat
  clock_ticks_per_second="$(getconf CLK_TCK)"
fi
disk_source=none
for disk_source_to_try in sysfs windows-helper ioreg; do
  case "$disk_source_to_try" in
    windows-helper) [ "$on_windows" = yes ] || continue ;;
    ioreg) command -v ioreg > /dev/null || continue ;;
  esac
  disk_source="$disk_source_to_try"
  read_disk_sample disk_sample
  [ "$disk_sample" = - ] || break
  disk_source=none
done
not_recorded=""
if [ "$machine_cpu_source" = none ]; then not_recorded="others"; fi
if [ "$disk_source" = none ]; then
  not_recorded="${not_recorded:+$not_recorded, }disk MB"
fi

# Runs one command under `side`'s binary and cache in round `round`, and
# appends a record to $records for each timing and work count it reports on
# stderr, then a `run` record of its wall, cpu, others and disk figures. A
# record holds the query index, side, round, record kind, name and value.
measure() {
  local query_index="$1" side="$2" round="$3" command="$4"
  shift 4
  local binary="$base_run_binary" cache="$base_run_cache"
  local disk_sample_before disk_sample_after machine_busy_before_ms machine_busy_after_ms
  local others_cpu_ms=- disk_bytes
  if [ "$side" = candidate ]; then
    binary="$candidate_run_binary"
    cache="$candidate_run_cache"
  fi
  # Take the disk samples outside the CPU samples: on Windows, the helper's
  # disk read would count as other processes' CPU. Between the CPU samples
  # are also the binary's start (bash's fork, or the helper's request and
  # process start) and the reading of its figures; between the disk samples
  # are also the CPU samples and, on macOS, ioreg's start and exit.
  read_disk_sample disk_sample_before
  read_machine_busy_ms machine_busy_before_ms
  "$runner" "$cache" "$binary" "$@"
  read_machine_busy_ms machine_busy_after_ms
  read_disk_sample disk_sample_after
  "$collect_run_stderr"
  if [ "$run_status" -ne 0 ]; then
    printf '%s binary failed: %s %s\n' "$side" "$binary" "$*" >&2
    cat "$run_stderr_file" >&2
    exit 1
  fi
  if [ "$machine_busy_before_ms" != - ]; then
    others_cpu_ms=$((machine_busy_after_ms - machine_busy_before_ms - run_cpu_ms - run_helper_cpu_ms))
  fi
  disk_bytes_between disk_bytes "$disk_sample_before" "$disk_sample_after"
  tr -d '\r' < "$run_stderr_file" | awk \
    -v prefix="$query_index\t$side\t$round\t" -v command="$command" '
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
  printf '%s\t%s\t%s\trun\t%s\t%s %s %s %s\n' "$query_index" "$side" "$round" "$command" \
    "$run_wall_ms" "$run_cpu_ms" "$others_cpu_ms" "$disk_bytes" >> "$records"
}

run_queries() {
  local side="$1" round="$2" index
  for index in "${!queries[@]}"; do
    measure "$index" "$side" "$round" agent-search agent search "${queries[$index]}" --mode lexical --perf-stats
    measure "$index" "$side" "$round" debug-search --debug-search "${queries[$index]}"
  done
}

echo "warm-up: filling or updating each binary's cache" >&2
run_queries base 0
run_queries candidate 0
: > "$records"
# Ask the OS to write the warm-up's cached writes to disk before round 1.
[ "$warm_up_sync" = skipped ] || sync

for round in $(seq 1 "$rounds"); do
  echo "round $round of $rounds" >&2
  # Alternating the order spreads a warmer OS file cache over both binaries.
  if [ $((round % 2)) -eq 1 ]; then
    run_queries base "$round"
    run_queries candidate "$round"
  else
    run_queries candidate "$round"
    run_queries base "$round"
  fi
done

# Prints the title, the figures not recorded, and a legend line for each
# recorded figure.
print_title_and_legend() {
  printf 'Medians over %d rounds, in milliseconds; speedup = base / candidate\n' "$rounds"
  [ -z "$not_recorded" ] || printf 'Not recorded on this machine: %s\n' "$not_recorded"
  printf 'process cpu base/cand: - when either median is under %s ms\n' "$cpu_ratio_floor_ms"
  printf '\nPer run: wall, cpu = the binary'\''s whole process, in ms\n'
  [ "$on_windows" = no ] || printf '  cpu is counted in %s ms ticks\n' "$cpu_step_ms"
  if [ "$machine_cpu_source" != none ]; then
    printf '  others = all cores'\'' busy time with kernel work, minus cpu, sampled just\n'
    printf '  before and after each run'
    if [ "$on_windows" = yes ]; then
      printf ', in %s ms ticks\n' "$cpu_step_ms"
    else
      printf ', in %s ms steps\n' "$((1000 / clock_ticks_per_second))"
      printf '  others leaves out steal, the CPU time a VM'\''s host gave other work\n'
    fi
    [ "$runner" = run_in_bash ] ||
      printf '  others also leaves out the CPU of the Windows helper, compare-perf-windows.ps1\n'
    printf '  others can be too high or too low, more so with more cpu and threads\n'
  fi
  if [ "$disk_source" != none ]; then
    printf '  disk MB = bytes every process read and wrote, sampled before and after\n'
    printf '  disk MB can include writes any process made before the run\n'
    printf '  disk MB - in one run: a sample was unreadable, a change in the set of disks\n'
    printf '  was detected, or the total decreased'
    [ "$disk_source" != ioreg ] || printf '; the disks are compared by count only'
    printf '\n'
    [ "$warm_up_sync" = waits ] ||
      printf '  early rounds'\'' disk MB can include the warm-up'\''s cache writes\n'
  fi
}
print_title_and_legend
printf '%s\n' "${queries[@]}" | awk -F '\t' -v rounds="$rounds" -v cpu_ratio_floor_ms="$cpu_ratio_floor_ms" '
  FNR == NR { query_text[NR - 1] = $0; query_count = NR; next }
  function add(query, kind_of_name, name, side, round, amount) {
    if (!((query, name) in kind)) {
      kind[query, name] = kind_of_name
      name_count[query]++
      name_at[query, name_count[query]] = name
    }
    # A stage that runs more than once in one run counts once, summed.
    if (!((query, name, side, round) in sum)) slot[query, name, side, round] = ++count[query, name, side]
    sum[query, name, side, round] += amount
    value[query, name, side, slot[query, name, side, round]] = sum[query, name, side, round]
  }
  $4 == "run" {
    query = $1; side = $2; round = $3; command = $5
    if (!((query, command) in has_command)) {
      has_command[query, command] = 1
      command_at[query, ++command_count[query]] = command
    }
    run_figures[query, command, side, round] = $6
    split($6, run_values, " ")
    add(query, "cpu", command, side, round, run_values[2])
    next
  }
  { add($1, $4, $5, $2, $3, $6) }
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
  function print_medians(query, wanted_kind, header, ratio_floor_ms,    i, name, base, candidate, ratio) {
    for (i = 1; i <= name_count[query]; i++) {
      name = name_at[query, i]
      if (kind[query, name] != wanted_kind || !compared(query, name)) continue
      base = median(query, name, "base")
      candidate = median(query, name, "candidate")
      ratio = "-"
      if (candidate > 0 && base >= ratio_floor_ms && candidate >= ratio_floor_ms) ratio = sprintf("%.2f", base / candidate)
      printf "%s  %-36s %10s %10s %10s\n", header, name, base, candidate, ratio
      header = ""
    }
  }
  function disk_megabytes(bytes) {
    if (bytes == "-") return "-"
    if (bytes > 0 && bytes < 50000) return "<0.1"
    return sprintf("%.1f", bytes / 1000000)
  }
  # Sets cells[1..4] to the run figures wall, cpu, others and disk MB.
  function fill_run_cells(query, command, side, round, cells) {
    cells[1] = cells[2] = cells[3] = cells[4] = "-"
    if ((query, command, side, round) in run_figures) split(run_figures[query, command, side, round], cells, " ")
    cells[4] = disk_megabytes(cells[4])
  }
  function print_runs(query,    round, i, command, base_cells, candidate_cells) {
    # Makes both arrays before fill_run_cells fills them: some awks cannot
    # turn an unused local into an array inside another function.
    split("", base_cells)
    split("", candidate_cells)
    printf "\n  %5s %-12s  %13s  %13s  %13s  %13s\n", "", "", "wall", "cpu", "others", "disk MB"
    printf "  %5s %-12s  %6s %6s  %6s %6s  %6s %6s  %6s %6s\n", "round", "command", \
      "base", "cand", "base", "cand", "base", "cand", "base", "cand"
    for (round = 1; round <= rounds; round++) {
      for (i = 1; i <= command_count[query]; i++) {
        command = command_at[query, i]
        fill_run_cells(query, command, "base", round, base_cells)
        fill_run_cells(query, command, "candidate", round, candidate_cells)
        printf "  %5s %-12s  %6s %6s  %6s %6s  %6s %6s  %6s %6s\n", round, command, \
          base_cells[1], candidate_cells[1], base_cells[2], candidate_cells[2], \
          base_cells[3], candidate_cells[3], base_cells[4], candidate_cells[4]
      }
    }
  }
  END {
    for (query = 0; query < query_count; query++) {
      notes = ""
      printf "\nquery: %s\n", query_text[query]
      printf "  %-36s %10s %10s %10s\n", "stage", "base", "candidate", "speedup"
      print_medians(query, "time", "", 0)
      print_medians(query, "cpu", sprintf("  %-36s %10s %10s %10s\n", "process cpu", "base", "candidate", "base/cand"), \
        cpu_ratio_floor_ms)
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
      print_runs(query)
    }
  }
' - "$records"
