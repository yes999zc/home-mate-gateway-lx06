# G6 serial helper (emOS has no adbd; this is the only device channel).
# Port name can change on USB re-enumeration: list /dev/cu.usbmodem* and
# pick the one that appears when the device boots (baseline the others).
# Usage: stability_log.py <out_log> [interval_min=5] [samples=12] [port]
#!/usr/bin/env python3
"""Pre-freeze account ③ (2026-11-08): 60-minute stability watch.

Every interval, three columns land in <out_log>:
  chunks_per_sec  — core /api/v0/mode mic telemetry (network path health)
  cpu_pct         — tools/cpu_pct.sh over a 10s window on the live firmware
                    PID (single-core convention, acceptance §0)
  dealias_locked  — last '[aec] dealias locked=' line from /run/hm-direct.log
Drop below 12 chunks/s is recorded as the raw number, never smoothed.
CPU clamped high while chunks drop = the signature the account hunts for;
this hour is also the capture window for the cross-boot ch8 race (§0.9).
"""
import json
import sys
import time
import urllib.request

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from fw_restart import Serial, first_pid  # noqa: E402

FIND_FW = (
    "for p in /proc/[0-9]*; do "
    '[ -r $p/cmdline ] || continue; '
    'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
    'case "$c" in *"current firmware"*) basename $p;; esac; '
    "done | head -1"
)


def chunks_per_sec() -> str:
    try:
        with urllib.request.urlopen("http://127.0.0.1:8930/api/v0/mode", timeout=5) as r:
            mic = json.load(r)["devices"][0]["mic"]
        return f"{mic['chunks_per_sec']:.2f}"
    except Exception as e:  # network hiccup is a finding, not a crash
        return f"ERR:{type(e).__name__}"


def main() -> int:
    out = sys.argv[1]
    interval_min = float(sys.argv[2]) if len(sys.argv) > 2 else 5.0
    samples = int(sys.argv[3]) if len(sys.argv) > 3 else 12
    port = sys.argv[4] if len(sys.argv) > 4 else "/dev/cu.usbmodem31101"
    ser = Serial(port)
    with open(out, "a") as f:
        f.write(f"# stability_log start {time.strftime('%Y-%m-%d %H:%M:%S')} "
                f"interval={interval_min}min samples={samples}\n")
        f.write("# time | chunks_per_sec | cpu_pct(10s,single-core) | dealias_locked\n")
        f.flush()
        for i in range(samples):
            ts = time.strftime("%H:%M:%S")
            chunks = chunks_per_sec()
            pid = first_pid(ser.cmd(FIND_FW, 3))
            if pid:
                cpu = ser.cmd(f"cd /data/local/tmp/hm-direct && sh cpu_pct.sh {pid} 10 1", 16)
                cpu_val = next(
                    (l.split(":")[1].split("%")[0].strip()
                     for l in cpu.splitlines() if "single-core" in l),
                    "ERR",
                )
                deal = ser.cmd("grep 'dealias locked' /run/hm-direct.log | tail -1", 4)
                lock = "unknown"
                for l in deal.splitlines():
                    if "locked=" in l:
                        seg = l.split("locked=")[1].split()[0]
                        lock = seg
            else:
                cpu_val, lock = "NO_FW_PROC", "NO_FW_PROC"
            f.write(f"{ts} | {chunks} | {cpu_val} | {lock}\n")
            f.flush()
            if i < samples - 1:
                time.sleep(interval_min * 60)
    return 0


if __name__ == "__main__":
    sys.exit(main())
