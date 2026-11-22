#!/system/bin/sh
# P0 watchdog evidence probe (2026-11-09). One line of key=value plus the
# active slot. Kept as a device-side script so the serial layers never have
# to quote a pipeline: the Mac watcher only sends `sh /data/p0/probe.sh`.
#
# U = uptime seconds
# T = timestamp of the newest `name:traps` line (unsupported compat syscall)
# N = its syscall number
# S = timestamp of the newest audio SPI timeout
# O = timestamp of the newest kernel fault
# A timestamp is empty when the ring holds no such line (aged out / none).
# Strip the 3-char <level> prefix dmesg adds before cutting the [sec.usec] field.
U=$(cut -d. -f1 /proc/uptime)

L=$(dmesg | grep 'name:traps' | tail -1 | cut -c4-)
T=$(printf '%s' "$L" | cut -c2-13 | tr -d ' ')
N=$(printf '%s' "$L" | grep -o 'syscall [0-9]*' | cut -d' ' -f2)

S=$(dmesg | grep 'SPI transfer timed out' | tail -1 | cut -c4- | cut -c2-13 | tr -d ' ')
O=$(dmesg | grep 'Unable to handle kernel' | tail -1 | cut -c4- | cut -c2-13 | tr -d ' ')

echo "U=$U T=$T N=$N S=$S O=$O"
readlink /data/local/tmp/hm-direct/current
