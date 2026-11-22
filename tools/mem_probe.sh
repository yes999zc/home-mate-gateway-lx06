#!/system/bin/sh
# Long-run memory probe (R0, 2026-11-12). One line of key=value so the Mac
# side never has to quote a pipeline over the serial console.
#
# U   = uptime seconds
# A   = MemAvailable kB (whole device)
# FW  = firmware VmRSS kB   (the armv7 audio process, cmdline ".../current firmware")
# C   = core VmRSS kB       (the aarch64 process, cmdline "/data/hm/bin/current")
# FWC = firmware fd count   (a leak shows here first on long runs)
# CC  = core fd count
# KWS = kws sidecar state as the firmware reports it ("running"/"down"/"-")
U=$(cut -d. -f1 /proc/uptime)
A=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
FW=""
CO=""
for p in /proc/[0-9]*; do
    [ -r "$p/cmdline" ] || continue
    c=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null)
    case "$c" in
        *"/current firmware "*) FW=$(basename "$p") ;;
        "/data/hm/bin/current ") CO=$(basename "$p") ;;
    esac
done
FWR=""
COR=""
FWC=""
CC=""
if [ -n "$FW" ]; then
    FWR=$(awk '/VmRSS/{print $2}' /proc/"$FW"/status 2>/dev/null)
    FWC=$(ls /proc/"$FW"/fd 2>/dev/null | wc -l)
fi
if [ -n "$CO" ]; then
    COR=$(awk '/VmRSS/{print $2}' /proc/"$CO"/status 2>/dev/null)
    CC=$(ls /proc/"$CO"/fd 2>/dev/null | wc -l)
fi
echo "U=$U A=$A FW=$FWR FWC=$FWC C=$COR CC=$CC"
