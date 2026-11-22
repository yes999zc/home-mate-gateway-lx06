# G6 serial helper (emOS has no adbd; this is the only device channel).
# Port name can change on USB re-enumeration: list /dev/cu.usbmodem* and
# pick the one that appears when the device boots (baseline the others).
# Usage: fw_restart.py [--port /dev/cu.usbmodem31101] [--timeout 45]
#!/usr/bin/env python3
"""Restart the hm-direct firmware the supervised way: TERM the main
supervisor shell (ppid=1, start_server.sh in cmdline) — its trap kills the
firmware child — then wait for init to respawn the script and a NEW
firmware process to appear. Prints the new PID, its cmdline, and the
boot/aec lines from /run/hm-direct.log. Exit 1 if no new process within
the timeout (leave diagnosis to the operator; never force-kill firmware).
"""
import argparse
import os
import sys
import termios
import time

FIND_SUP = (
    "for p in /proc/[0-9]*; do "
    '[ -r $p/cmdline ] || continue; '
    'pp=$(awk "{print \\$4}" $p/stat 2>/dev/null); '
    'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
    'case "$c" in *start_server.sh*) [ "$pp" = "1" ] && basename $p;; esac; '
    "done | head -1"
)
FIND_FW = (
    "for p in /proc/[0-9]*; do "
    '[ -r $p/cmdline ] || continue; '
    'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
    'case "$c" in *"current firmware"*) basename $p;; esac; '
    "done | head -1"
)


def first_pid(reply: str) -> str:
    """The tty echoes the command with control sequences; only lines that
    are a bare number count (transient /proc races drop noise lines)."""
    for line in reply.splitlines():
        s = line.strip()
        if s.isdigit():
            return s
    return ""


class Serial:
    def __init__(self, port: str):
        self.fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        a = termios.tcgetattr(self.fd)
        a[3] &= ~(termios.ECHO | termios.ICANON | termios.ISIG)
        termios.tcsetattr(self.fd, termios.TCSANOW, a)

    def cmd(self, command: str, wait: float = 4.0) -> str:
        # Drain pending output first so replies are not mixed.
        deadline = time.time() + 0.5
        while time.time() < deadline:
            try:
                if not os.read(self.fd, 4096):
                    break
            except BlockingIOError:
                break
        # Write ALL bytes: the fd is non-blocking, so a single os.write can
        # land partially (tty buffer full) and silently drop the rest — a
        # 1 KiB serial_push chunk produced an empty file exactly this way.
        payload = (command + "\n").encode()
        while payload:
            try:
                n = os.write(self.fd, payload)
                payload = payload[n:]
            except BlockingIOError:
                time.sleep(0.05)
        out = b""
        end = time.time() + wait
        while time.time() < end:
            try:
                c = os.read(self.fd, 65536)
                if c:
                    out += c
            except BlockingIOError:
                time.sleep(0.05)
        return out.decode(errors="replace")

    def close(self):
        os.close(self.fd)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default="/dev/cu.usbmodem31101")
    ap.add_argument("--timeout", type=float, default=45.0)
    args = ap.parse_args()

    ser = Serial(args.port)
    try:
        old_fw = first_pid(ser.cmd(FIND_FW, 3)) or "?"
        # Supervisor = the PARENT of the running firmware. The generic
        # FIND_SUP (start_server.sh with ppid=1, head -1) keeps matching
        # ORPHANED log-trim subshells reparented to init — their cmdline is
        # still start_server.sh but they never own the firmware, so TERM
        # lands on a no-op and the restart times out (bit three times on
        # 2026-11-10). Owning-parent targeting cannot pick a wrong shell.
        sup = first_pid(ser.cmd(
            f"[ -d /proc/{old_fw} ] && awk '{{print $4}}' /proc/{old_fw}/stat", 3
        )) if old_fw != "?" else ""
        if not sup or sup == "1":
            sup = first_pid(ser.cmd(FIND_SUP, 3))
        if not sup:
            print("no main supervisor found; aborting")
            return 1
        print(f"supervisor={sup} firmware_was={old_fw}")
        ser.cmd(f"kill -TERM {sup}", 2)

        deadline = time.time() + args.timeout
        new_fw = ""
        while time.time() < deadline:
            cand = first_pid(ser.cmd(FIND_FW, 3))
            if cand and cand != old_fw:
                new_fw = cand
                break
            time.sleep(1)
        if not new_fw:
            print(f"ERROR: no new firmware process within {args.timeout}s")
            print(ser.cmd("tail -5 /data/local/tmp/hm-direct/supervisor.log", 3))
            return 1
        print(f"firmware_now={new_fw}")
        info = ser.cmd(
            f"tr '\\0' ' ' < /proc/{new_fw}/cmdline; echo; "
            f"grep -E 'aec|connected|generation' /run/hm-direct.log | tail -5",
            3,
        )
        print(info.strip())
        return 0
    finally:
        ser.close()


if __name__ == "__main__":
    sys.exit(main())
