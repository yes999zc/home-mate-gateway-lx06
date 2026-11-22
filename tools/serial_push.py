# Push a small file to the emOS device over the ACM serial console.
# The Mac's application firewall drops LAN inbound for ad-hoc servers
# (G4 lesson), so http.server + wget is not always available; the serial
# tty carries base64 chunks fine (USB ACM is not baud-bound).
# Usage: serial_push.py <local_file> <device_path> [port]
import base64
import hashlib
import os
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from fw_restart import Serial  # noqa: E402

CHUNK = 1024  # base64 chars per write — mksh line limits stay far away


def main() -> int:
    local, remote = sys.argv[1], sys.argv[2]
    port = sys.argv[3] if len(sys.argv) > 3 else "/dev/cu.usbmodem31101"
    data = open(local, "rb").read()
    md5 = hashlib.md5(data).hexdigest()
    b64 = base64.b64encode(data).decode()
    ser = Serial(port)
    ser.cmd(f": > {remote}.b64", 1.5)
    n = (len(b64) + CHUNK - 1) // CHUNK
    for i in range(n):
        part = b64[i * CHUNK:(i + 1) * CHUNK]
        ser.cmd(f"printf '%s' '{part}' >> {remote}.b64", 1.0)
        if i % 16 == 15:
            print(f"chunk {i + 1}/{n}")
    out = ser.cmd(f"busybox base64 -d {remote}.b64 > {remote} && rm {remote}.b64 && md5sum {remote}", 5)
    ser.close()
    ok = md5 in out
    print(out.strip())
    print(f"expect md5 {md5} -> {'OK' if ok else 'MISMATCH'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
