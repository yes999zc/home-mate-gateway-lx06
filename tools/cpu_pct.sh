#!/system/bin/sh
# Single-core CPU percentage of a process, sampled over an interval.
# Definition (G6 acceptance §0, user 2026-11-08): the denominator is ONE
# core, never the four — single-core % = (Δutime+Δstime)/Δwall × 100.
# utime/stime come from /proc/<pid>/stat fields 14/15 in clock ticks
# (CLK_TCK=100 on this kernel); wall clock from /proc/uptime.
# Runs on-device (emOS /system/bin/sh or FireOS) with no extra tools.
#
#   cpu_pct.sh <pid> [interval_secs=10] [runs=1]
pid=${1:?usage: cpu_pct.sh <pid> [interval_secs] [runs]}
interval=${2:-10}
runs=${3:-1}

ticks() { awk '{print $14+$15}' /proc/$pid/stat 2>/dev/null; }
uptime() { cut -d. -f1 /proc/uptime; }

for r in $(seq 1 $runs); do
    t0=$(ticks); w0=$(uptime)
    [ -n "$t0" ] || { echo "no such process $pid"; exit 1; }
    sleep $interval
    t1=$(ticks); w1=$(uptime)
    dt=$(( t1 - t0 )); dw=$(( w1 - w0 ))
    [ "$dw" -gt 0 ] || { echo "no wall time elapsed"; exit 1; }
    # ticks/100 = seconds; ×100 for percent; ÷dw seconds over the interval
    pct=$(awk -v dt=$dt -v dw=$dw -v iv=$interval 'BEGIN { printf "%.1f", dt / dw }')
    echo "run $r: ${pct}% single-core (Δticks=$dt over ${dw}s)"
done
