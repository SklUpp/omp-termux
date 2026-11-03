#!/data/data/com.termux/files/usr/bin/bash
# Reproducible startup/memory benchmark for OMP on Termux Android arm64.
#
# Why this exists: plan targets ("116 MB -> X", "697 ms -> Y") are unfalsifiable
# without a fixed harness. Ad-hoc `date +%s%N` loops on a phone that is also
# running agents drift by ~15%. This script fixes the method so before/after
# numbers are comparable.
#
# Usage:
#   android/scripts/bench.sh                 # bench the installed omp
#   android/scripts/bench.sh --runs 9        # more samples
#   android/scripts/bench.sh --json          # machine-readable
#
# Reports median (not mean: one scheduler hiccup skews a mean, not a median).
set -uo pipefail

LIB_DIR="${OMP_LIB_DIR:-/data/data/com.termux/files/usr/lib/omp-termux}"
RUNS=5
JSON=0

while [ $# -gt 0 ]; do
	case "$1" in
		--runs) RUNS="$2"; shift 2 ;;
		--json) JSON=1; shift ;;
		--lib) LIB_DIR="$2"; shift 2 ;;
		-h|--help) sed -n '2,14p' "$0"; exit 0 ;;
		*) echo "unknown arg: $1" >&2; exit 2 ;;
	esac
done

BUN="$LIB_DIR/bun"
CLI="$LIB_DIR/cli.js"
for f in "$BUN" "$CLI"; do
	[ -x "$BUN" ] && [ -r "$CLI" ] || { echo "missing $f" >&2; exit 1; }
done

# Exclude any already-running agent session from RSS sampling: summing every
# `bun` process would silently add a live 390 MB session to the measurement.
SELF_EXCLUDE="${OMP_BENCH_EXCLUDE_PID:-0}"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }

# --- wall time -------------------------------------------------------------
time_ms() {
	local s e
	s=$(date +%s%N)
	"$@" >/dev/null 2>&1
	e=$(date +%s%N)
	echo $(( (e - s) / 1000000 ))
}

# --- peak RSS of the child only --------------------------------------------
# Sampling /proc at 20 ms: a ~600 ms startup gives ~30 samples, enough for the
# peak plateau. Short-lived children may exit before the first read; that is
# reported as 0 rather than silently guessed.
peak_rss_kb() {
	"$@" >/dev/null 2>&1 &
	local pid=$! peak=0 cur
	while kill -0 "$pid" 2>/dev/null; do
		cur=$(ps -eo pid,rss,comm 2>/dev/null \
			| awk -v skip="$SELF_EXCLUDE" -v me="$pid" '$1==me || ($3=="bun" && $1!=skip && $1==me) {s+=$2} END{print s+0}')
		[ "${cur:-0}" -gt "$peak" ] 2>/dev/null && peak=$cur
		sleep 0.02
	done
	wait "$pid" 2>/dev/null
	echo "$peak"
}

# --- addon residency -------------------------------------------------------
# The 151 MB .node is mmap'd, so VSZ is meaningless; only smaps Rss shows what
# the kernel actually faulted in. This is the check that proves lazy-import work.
addon_rss_kb() {
	"$@" >/dev/null 2>&1 &
	local pid=$! best=0 cur
	while kill -0 "$pid" 2>/dev/null; do
		cur=$(awk '/pi_natives/{f=1} /^[0-9a-f]+-[0-9a-f]+ /{if($0 !~ /pi_natives/) f=0} f && /^Rss:/{s+=$2} END{print s+0}' \
			"/proc/$pid/smaps" 2>/dev/null)
		[ "${cur:-0}" -gt "$best" ] 2>/dev/null && best=$cur
		sleep 0.02
	done
	wait "$pid" 2>/dev/null
	echo "$best"
}

run_omp() { OMP_PLATFORM=android "$BUN" "$CLI" "$@"; }

times=() rss=()
for _ in $(seq 1 "$RUNS"); do times+=("$(time_ms run_omp --version)"); done
for _ in $(seq 1 "$RUNS"); do rss+=("$(peak_rss_kb run_omp --version)"); done

T=$(median "${times[@]}")
R=$(median "${rss[@]}")
A=$(addon_rss_kb run_omp --version)
BASE=$("$BUN" -e 'console.log(+require("fs").readFileSync("/proc/self/status","utf8").match(/VmHWM:\s+(\d+)/)[1])' 2>/dev/null || echo 0)

if [ "$JSON" = "1" ]; then
	printf '{"runs":%s,"version_ms_median":%s,"version_rss_kb_median":%s,"addon_rss_kb":%s,"bun_baseline_kb":%s,"samples_ms":"%s"}\n' \
		"$RUNS" "$T" "$R" "$A" "$BASE" "${times[*]}"
else
	echo "OMP Termux benchmark  (runs=$RUNS, median)"
	echo "  --version wall        : ${T} ms      samples: ${times[*]}"
	echo "  --version peak RSS    : ${R} kB"
	echo "  pi_natives resident   : ${A} kB      <- must be 0 after lazy-import work"
	echo "  bun baseline HWM      : ${BASE} kB"
	echo
	echo "note: phone scheduling adds ~15% jitter; compare medians, same device, same load."
fi
