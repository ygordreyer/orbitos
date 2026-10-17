#!/bin/sh
# BusyBox/POSIX sh, on-device only. Writes samples and extracted trace into OUT.
# Usage: sh measure-lifecycle.sh OUT [interval_seconds [samples [trace_log]]]
# Start launcher with /tmp/allium-perf-trace already present. This script never
# enables tracing, changes governors, sends signals or modifies either process.
# Compare monotonic_us at launcher_entry / first_completed_flush, and launch_prep /
# imminent_exec. Frame stages.phases has per-input inclusive wall time; measurement
# separates frame totals from input totals and first_feedback/final_content.
# CPU seconds = delta(utime_ticks + stime_ticks) / clock-ticks-per-second.txt.
# Compare only matching PID/starttime pairs, since launcher can restart.
# perf-stages.jsonl is the retained rolling trace window at sampler exit. Keep
# stage captures short enough to retain both endpoints; no log rotation is changed.
set -eu
[ "$#" -ge 1 ] && [ "$#" -le 4 ] || { echo "usage: $0 OUT [interval_seconds [samples [trace_log]]]" >&2; exit 2; }
out=$1
interval=${2:-0.1}
samples=${3:-300}
trace=${4:-/tmp/allium-perf-trace.log}
case $samples in ''|*[!0-9]*|0) echo 'samples must be a positive integer' >&2; exit 2;; esac
awk -v n="$interval" 'BEGIN { exit !(n ~ /^[0-9]+([.][0-9]+)?$/ && n+0 > 0) }' || { echo 'interval must be positive seconds' >&2; exit 2; }
# Refuse existing output: evidence must never be silently overwritten.
[ ! -e "$out" ] || { echo 'OUT already exists' >&2; exit 2; }
mkdir -p "$out"
getconf CLK_TCK > "$out/clock-ticks-per-second.txt" 2>/dev/null || echo unknown > "$out/clock-ticks-per-second.txt"
printf '%s\n' 'uptime_s kind name pid tid metrics' > "$out/samples.txt"
cleanup() { extract_trace; }
extract_trace() {
    if [ -r "$trace" ]; then
        # Preserve original JSON. Selection, task, lifecycle and frame rows share
        # CLOCK_MONOTONIC with /proc/uptime (suspend offsets can differ).
        awk '/"event"|"menu_phase"|"stages"/ {print}' "$trace" > "$out/perf-stages.jsonl"
    fi
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
sample_process() {
    name=$1
    pid=$2
    [ -r "/proc/$pid/status" ] || return 0
    awk -v now="$now" -v name="$name" -v pid="$pid" '
        /^(VmRSS|VmHWM|Threads|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):/ {printf "%s status %s %s - %s %s %s\n",now,name,pid,$1,$2,$3}
    ' "/proc/$pid/status" >> "$out/samples.txt" 2>/dev/null || :
    # comm may contain spaces or parentheses. Strip through the final ") ".
    # Remaining fields start at stat field 3, so utime/stime are 12/13.
    for stat in "/proc/$pid/stat" /proc/"$pid"/task/*/stat; do
        [ -r "$stat" ] || continue
        tid=${stat%/stat}; tid=${tid##*/}
        [ "$stat" != "/proc/$pid/stat" ] || tid=-
        if [ "$tid" != - ] && [ -r "/proc/$pid/task/$tid/status" ]; then
            awk -v now="$now" -v name="$name" -v pid="$pid" -v tid="$tid" '
                /^(voluntary_ctxt_switches|nonvoluntary_ctxt_switches):/ {printf "%s thread_status %s %s %s %s %s\n",now,name,pid,tid,$1,$2}
            ' "/proc/$pid/task/$tid/status" >> "$out/samples.txt" 2>/dev/null || :
        fi
        awk -v now="$now" -v name="$name" -v pid="$pid" -v tid="$tid" '
            {sub(/^.*\) /, ""); printf "%s stat %s %s %s utime_ticks=%s stime_ticks=%s minflt=%s majflt=%s starttime_ticks=%s\n", now,name,pid,tid,$12,$13,$8,$10,$20}
        ' "$stat" >> "$out/samples.txt" 2>/dev/null || :
    done
    if [ -r "/proc/$pid/io" ]; then
        awk -v now="$now" -v name="$name" -v pid="$pid" '{printf "%s io %s %s - %s %s\n",now,name,pid,$1,$2}' "/proc/$pid/io" >> "$out/samples.txt" 2>/dev/null || :
    fi
}
i=0
while [ "$i" -lt "$samples" ]; do
    read -r now rest < /proc/uptime
    awk -v now="$now" '/^MemAvailable:/ {printf "%s memory - - - MemAvailable_kB=%s\n",now,$2}' /proc/meminfo >> "$out/samples.txt"
    for freq in /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq; do
        [ -r "$freq" ] || continue
        read -r khz < "$freq" || continue
        printf '%s cpufreq - - - %s=%s_kHz\n' "$now" "$freq" "$khz" >> "$out/samples.txt"
    done
    # Resolve every sample so launcher exit/reentry and daemon restarts are visible.
    for name in allium-launcher alliumd; do
        pids=$(pidof "$name" 2>/dev/null || :)
        for pid in $pids; do sample_process "$name" "$pid"; done
    done
    i=$((i + 1))
    [ "$i" -ge "$samples" ] || sleep "$interval"
done
# Interval is requested sleep plus sampler overhead, recorded uptime is the truth.
