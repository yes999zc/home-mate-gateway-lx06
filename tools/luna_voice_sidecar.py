#!/usr/bin/env python3
"""Deterministic KWS protocol partner, not a speech-recognition model.

Run by actual Firmware's sidecar supervisor from an isolated --kws-dir.
A keyword.json command is consumed only after nonzero PCM arrives on stdin.
The partner never constructs northbound Gateway frames.
"""
import hashlib
import json
import os
from pathlib import Path
import sys


def main():
    total = nonzero = 0
    digest = hashlib.sha256()
    print(json.dumps({"event": "ready", "load_ms": 1}), flush=True)
    while True:
        pcm = os.read(sys.stdin.fileno(), 2560)
        if not pcm:
            return
        total += len(pcm)
        digest.update(pcm)
        if any(pcm):
            nonzero += len(pcm)
        command = Path("keyword.json")
        if any(pcm) and command.exists():
            request = json.loads(command.read_text())
            command.unlink()
            observed = {"pcm_bytes": total, "nonzero_bytes": nonzero,
                        "pcm_sha256": digest.hexdigest(), "request": request,
                        "pid": os.getpid()}
            temporary = Path("observed.tmp")
            temporary.write_text(json.dumps(observed))
            temporary.replace("observed.json")
            print(json.dumps({"event": "keyword", "keyword": request["keyword"],
                              "fed_ms": total // 32, "lag_ms": 0}), flush=True)


if __name__ == "__main__":
    main()
