#!/usr/bin/env python3
"""D 1.6 companion: sample device memory during the soak (one serial line each).

`/proc/meminfo` is not exposed by the management API, and the soak driver must
not touch the serial line (p0_watch owns it at 20 s). This sampler reads one
line every `--every` seconds and appends `mac_time uptime_s MemAvailable_kB`
to a TSV. A missed sample (port busy / device restarting) is written as `-1`
and is not retried — a hole in the curve is honest, a silent gap is not.

Usage: soak_mem.py <out.tsv> [--minutes 200] [--every 600] [--port ...]
"""
import argparse
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SERIAL = os.path.join(HERE, "serial_cmd.py")


def probe(path_script="/data/hm/mem_probe.sh"):
    """One round trip through the device-side probe (tools/mem_probe.sh).

    Returns (uptime, avail_kb, fw_rss_kb, fw_fd, core_rss_kb, core_fd) with
    None for anything the device did not answer. The probe exists because the
    interesting question on a multi-hour idle run is not just MemAvailable but
    whether the two long-lived processes (firmware, core) and their fd counts
    move at all."""
    try:
        out = subprocess.run(
            [sys.executable, SERIAL, f"sh {path_script}", "5"],
            capture_output=True, text=True, timeout=40).stdout
    except Exception:  # noqa: BLE001
        return (None,) * 6
    kv = {}
    for tok in out.replace("\n", " ").split():
        k, _, v = tok.partition("=")
        if k in ("U", "A", "FW", "FWC", "C", "CC"):
            kv[k] = int(v) if v.isdigit() else None
    return (kv.get("U"), kv.get("A"), kv.get("FW"), kv.get("FWC"),
            kv.get("C"), kv.get("CC"))


def sample(port):
    try:
        # R0 (2026-11-12): `;` returns an empty reply on this console
        # (mksh over ttyGS0 echoes the line and the second command's output
        # is lost through serial_cmd's echo-strip); `&&` returns both values.
        out = subprocess.run(
            [sys.executable, SERIAL, 'grep MemAvailable /proc/meminfo && cut -d. -f1 /proc/uptime',
             "5", port],
            capture_output=True, text=True, timeout=40).stdout
    except Exception:  # noqa: BLE001
        return None, None
    avail = up = None
    for line in out.splitlines():
        if "MemAvailable" in line:
            for tok in line.replace("MemAvailable:", " ").split():
                if tok.isdigit():
                    avail = int(tok)
                    break
        elif line.strip().isdigit() and up is None:
            up = int(line.strip())
    return avail, up


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--minutes", type=float, default=200)
    ap.add_argument("--every", type=float, default=600)
    ap.add_argument("--port", default="/dev/cu.usbmodem31101")
    ap.add_argument("--probe", action="store_true",
                    help="also record firmware/core VmRSS and fd counts via tools/mem_probe.sh")
    a = ap.parse_args()
    end = time.time() + a.minutes * 60
    with open(a.out, "a", buffering=1) as f:
        if a.probe:
            f.write("# mac_time\tuptime_s\tMemAvailable_kB\tfw_rss_kB\tfw_fd\tcore_rss_kB\tcore_fd\n")
        else:
            f.write("# mac_time\tuptime_s\tMemAvailable_kB\n")
        while time.time() < end:
            if a.probe:
                up, avail, fw, fwc, co, cc = probe()
                f.write(f"{time.strftime('%H:%M:%S')}\t{up if up is not None else -1}\t"
                        f"{avail if avail is not None else -1}\t{fw if fw is not None else -1}\t"
                        f"{fwc if fwc is not None else -1}\t{co if co is not None else -1}\t"
                        f"{cc if cc is not None else -1}\n")
                print(f"{time.strftime('%H:%M:%S')} up={up} avail={avail} "
                      f"fw={fw} fw_fd={fwc} core={co} core_fd={cc}", flush=True)
            else:
                avail, up = sample(a.port)
                f.write(f"{time.strftime('%H:%M:%S')}\t{up if up is not None else -1}\t"
                        f"{avail if avail is not None else -1}\n")
                print(f"{time.strftime('%H:%M:%S')} up={up} avail_kB={avail}", flush=True)
            time.sleep(a.every)
    return 0


if __name__ == "__main__":
    sys.exit(main())
