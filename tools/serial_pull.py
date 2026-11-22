# G6 serial helper (emOS has no adbd; this is the only device channel).
# Port name can change on USB re-enumeration: list /dev/cu.usbmodem* and
# pick the one that appears when the device boots (baseline the others).
# Usage: serial_pull.py "<shell command>" [wait_secs] [port]
#!/usr/bin/env python3
"""Pull a binary file off the emOS console over the ACM serial line.

Sends `busybox base64 <path>` and collects the base64 text until the shell
prompt returns, then decodes locally. Slow (~tens of KB/s) but the only
device->Mac channel without adb. Chunked echo keeps the tty from choking.
"""
import base64
import os
import sys
import termios
import time

path = sys.argv[1]
out = sys.argv[2]
port = sys.argv[3] if len(sys.argv) > 3 else "/dev/cu.usbmodem31101"

fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
a = termios.tcgetattr(fd)
a[3] &= ~(termios.ECHO | termios.ICANON | termios.ISIG)
termios.tcsetattr(fd, termios.TCSANOW, a)


def drain(secs: float) -> bytes:
    buf = b""
    end = time.time() + secs
    while time.time() < end:
        try:
            c = os.read(fd, 65536)
            if c:
                buf += c
        except BlockingIOError:
            time.sleep(0.05)
    return buf


drain(0.3)
os.write(fd, f"busybox base64 {path}\n".encode())
text = b""
end = time.time() + 240
while time.time() < end:
    chunk = drain(1.0)
    if chunk:
        text += chunk
        if b"root@" in chunk.split(b"\n")[-1]:
            break
os.close(fd)
# Keep only base64-looking lines (drop echo of the command and the prompt).
lines = []
for line in text.decode(errors="replace").splitlines():
    s = line.strip()
    if not s or "busybox" in s or s.startswith("root@"):
        continue
    lines.append(s)
data = base64.b64decode("".join(lines))
with open(out, "wb") as f:
    f.write(data)
print(f"pulled {len(data)} bytes -> {out}")
