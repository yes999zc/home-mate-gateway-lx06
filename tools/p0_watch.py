#!/usr/bin/env python3
"""P0 watchdog-reset watcher (2026-11-09).

The board resets every few minutes with `androidboot.bootreason=wdt_by_pass_pwk`.
The crash scene lives only in the previous boot's `/proc/last_kmsg`, and the
kernel log ring is bounded, so this tool samples the **age of the newest**
unsupported-syscall trap dump (plus its syscall number), SPI timeout and
kernel fault instead of a total: a saturated ring keeps a constant count even
while new dumps arrive, but an age that keeps growing proves nothing new
landed. Uptime, the slot currently linked as `current`, and a falling uptime
(reset) complete the picture.

Every sample is one serial round trip (the device's only live channel). A
falling uptime is recorded as a reset; a missing port is recorded as a
reconnect wait, not a reset, so a flaky USB re-enumeration cannot invent one.

Usage: p0_watch.py <out.tsv> [duration_min=60] [interval_s=20] [port]
Output columns: mac_time  uptime_s  trap_age_s  trap_no  spi_age_s  oops_age_s  slot  event

The 2026-11-09 investigation: docs/d/p0-watchdog-resets-20261109.md.

The device-side probe lives in tools/p0_probe.sh; push it once with
`serial_push.py tools/p0_probe.sh /data/p0/probe.sh`.
"""
import datetime
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SERIAL_CMD = os.path.join(HERE, "serial_cmd.py")
PROBE = "sh /data/p0/probe.sh"


def sample(port: str, wait: float = 5.0):
    """One device round trip.

    Returns (uptime, trap_age, trap_no, spi_age, oops_age, slot) where the
    ages are seconds since the newest matching kernel line, or -1 when the
    ring holds none (aged out or never happened). Ages, not totals: the
    kernel log ring is bounded, so a total saturates and cannot prove
    "nothing new landed". The probe itself is a device-side script so the
    serial layers never have to quote a pipeline.
    """
    try:
        out = subprocess.run(
            [sys.executable, SERIAL_CMD, PROBE, str(wait), port],
            capture_output=True,
            text=True,
            timeout=wait + 20,
        ).stdout
    except Exception:
        return None
    kv = {}
    for tok in out.replace("\n", " ").split():
        if "=" in tok:
            k, _, v = tok.partition("=")
            kv[k] = v
    slot = ""
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("audio_") or line.startswith("core_"):
            slot = line
    if "U" not in kv or not kv["U"].isdigit():
        return None
    up = int(kv["U"])

    def age(key: str) -> int:
        v = kv.get(key, "")
        try:
            # The kernel timestamp is seconds.microseconds since boot.
            return up - int(float(v))
        except ValueError:
            return -1

    trap_no = kv.get("N", "")
    return up, age("T"), trap_no, age("S"), age("O"), slot


def main() -> int:
    out_path = sys.argv[1]
    minutes = float(sys.argv[2]) if len(sys.argv) > 2 else 60.0
    interval = float(sys.argv[3]) if len(sys.argv) > 3 else 20.0
    port = sys.argv[4] if len(sys.argv) > 4 else "/dev/cu.usbmodem31101"

    deadline = time.time() + minutes * 60
    prev_up = None
    prev_trap_age = None
    resets = 0
    new_traps = 0   # the criterion, not the raw count: see the note below
    peak_up = 0
    samples = 0
    new_file = not os.path.exists(out_path)
    with open(out_path, "a") as f:
        if new_file:
            f.write("# mac_time\tuptime_s\ttrap_age_s\ttrap_no\tspi_age_s\toops_age_s\tslot\tevent\n")
        f.write(f"# start {datetime.datetime.now().isoformat(timespec='seconds')} "
                f"duration={minutes}min interval={interval}s port={port}\n")
        f.flush()
        while time.time() < deadline:
            now = datetime.datetime.now()
            got = sample(port)
            if got is None:
                f.write(f"{now.strftime('%H:%M:%S')}\t-\t-\t-\t-\t-\t-\tno_port\n")
                f.flush()
                time.sleep(interval)
                continue
            up, trap_age, trap_no, spi_age, oops_age, slot = got
            event = ""
            # R0 lesson: a bounded kernel ring makes the raw trap COUNT
            # saturate and even fall — "it got better" is an artifact. The
            # only honest signal is the AGE of the newest dump: if it falls
            # from one sample to the next, a NEW trap arrived. This counter
            # is the PASS/FAIL datum (0 = no new unsupported-syscall dumps).
            if prev_trap_age is not None and trap_age >= 0 and trap_age < prev_trap_age:
                new_traps += 1
                event = f"NEW TRAP ({prev_trap_age}s -> {trap_age}s, no {trap_no})"
            prev_trap_age = trap_age if trap_age >= 0 else prev_trap_age
            if prev_up is not None and up < prev_up:
                resets += 1
                event = f"RESET (prev {prev_up}s)"
            elif prev_up is not None and up > prev_up + interval * 3:
                event = f"gap {up - prev_up}s"
            prev_up = up
            peak_up = max(peak_up, up)
            samples += 1
            f.write(f"{now.strftime('%H:%M:%S')}\t{up}\t{trap_age}\t{trap_no}\t"
                    f"{spi_age}\t{oops_age}\t{slot}\t{event}\n")
            f.flush()
            time.sleep(interval)
    print(f"samples={samples} resets={resets} new_traps={new_traps} peak_uptime={peak_up}s")
    if new_traps:
        print(f"FAIL: {new_traps} new unsupported-syscall trap dump(s) in the window")
    else:
        print("PASS: no new unsupported-syscall trap dumps (trap age only grew)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
