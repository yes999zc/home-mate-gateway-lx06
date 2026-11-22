#!/usr/bin/env python3
"""Observe actual Firmware capture and sidecar events at its TCP consumer."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


def run(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="voice-fixture-", dir=output.resolve()))
    partner = Path(__file__).with_name("luna_voice_sidecar.py").resolve()
    # Avoid shell interpolation of source paths; exec Python with a literal argv.
    (root / "run.sh").write_text(f"#!{sys.executable}\nimport os\nos.execv({sys.executable!r}, [{sys.executable!r}, {str(partner)!r}])\n")
    (root / "run.sh").chmod(0o700)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(5)
    frames = []
    result = {"status": "failed", "scope": "actual-firmware-sidecar-capture",
              "recognition_model": "unexercised", "checks": [], "protocol": 0,
              "fixture_binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=partner.parent.parent, text=True).strip(),
              "source_files": {name: hashlib.sha256((partner.parent.parent / name).read_bytes()).hexdigest()
                  for name in ("crates/gateway/examples/northbound_fixture.rs", "tools/luna_voice_sidecar.py", "tools/test_voice_fixture.py")}}
    child = connection = None
    try:
        with (root / "gateway.log").open("wb") as log:
            child = subprocess.Popen([str(binary.resolve()), "--core", f"127.0.0.1:{listener.getsockname()[1]}",
                "--state-dir", str(root / "state"), "--device", "voice-fixture", "--kws-dir", str(root),
                "--probe-file", str(root / "probe.jsonl")], stdin=subprocess.PIPE, stdout=log,
                stderr=subprocess.STDOUT, text=True, env={"PATH": os.defpath}, start_new_session=True)
            child.stdin.write("capture silence\n")
            child.stdin.flush()
            connection, _ = listener.accept()
            connection.settimeout(1)
            reader = connection.makefile("rb")
            hello = json.loads(reader.readline(262145))
            assert hello["type"] == "hello", hello
            connection.sendall(b'{"type":"hello_ack","protocol":0,"min_protocol":0,"core_version":"voice-fixture","generation":1}\n')
            connection.sendall(b'{"type":"mic_session","device":"voice-fixture","action":"start","mode":"continuous"}\n')
            deadline = time.monotonic() + 15
            quiet = loud = False
            command_sent = False
            keyword = None
            ending = False
            trailing_quiet = 0
            with (root / "frames.jsonl").open("w") as evidence:
                while time.monotonic() < deadline:
                    frame = json.loads(reader.readline(262145))
                    frames.append(frame)
                    evidence.write(json.dumps(frame) + "\n")
                    if frame["type"] == "audio_chunk":
                        pcm = base64.b64decode(frame["pcm_b64"])
                        if not any(pcm):
                            if ending:
                                trailing_quiet += 1
                            quiet = True
                            if not command_sent:
                                (root / "keyword.json").write_text(json.dumps({"keyword": "你好高达"}))
                                child.stdin.write("capture speech\n")
                                child.stdin.flush()
                                command_sent = True
                        else:
                            trailing_quiet = 0
                            loud = True
                    if frame["type"] == "keyword":
                        assert frame["keyword"] == "你好高达", frame
                        assert frame["device"] == "voice-fixture", frame
                        keyword = frame
                    if keyword and quiet and loud and not ending:
                        child.stdin.write("capture silence\n")
                        child.stdin.flush()
                        ending = True
                    if ending and trailing_quiet >= 8:
                        observed = json.loads((root / "observed.json").read_text())
                        assert observed["nonzero_bytes"] > 0 and observed["pcm_bytes"] >= observed["nonzero_bytes"]
                        assert quiet and loud, "actual capture consumer did not see both PCM modes"
                        result["sidecar"] = observed
                        result["keyword"] = keyword
                        result["trailing_quiet_blocks"] = trailing_quiet
                        result["checks"] = ["actual Firmware Hello/ack", "silent and nonzero capture at TCP consumer",
                                            "PCM-fed sidecar keyword consumed by actual Firmware and forwarded over TCP",
                                            "speech returns to at least eight actual silent PCM blocks"]
                        result["status"] = "passed"
                        break
                assert result["status"] == "passed", "no actual Firmware keyword before deadline"
            reader.close()
    except Exception as error:
        result["error"] = str(error)
    finally:
        if connection:
            connection.close()
        listener.close()
        if child:
            try:
                child.stdin.write("shutdown\n")
                child.stdin.flush()
                child.wait(timeout=3)
            except (OSError, subprocess.TimeoutExpired):
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=3)
            # Firmware's example exits its process rather than joining its
            # supervisor. Observe sidecar EOF exit, then bound any residuals
            # in the independent process group even after the parent exits 0.
            def group_alive():
                try:
                    os.killpg(child.pid, 0)
                    return True
                except ProcessLookupError:
                    return False
            until = time.monotonic() + 3
            while group_alive() and time.monotonic() < until:
                time.sleep(.02)
            natural = not group_alive()
            if not natural:
                os.killpg(child.pid, signal.SIGKILL)
                until = time.monotonic() + 3
                while group_alive() and time.monotonic() < until:
                    time.sleep(.02)
            result["sidecar_cleanup"] = {"group_exited_after_eof": natural,
                "forced_group_kill": not natural, "group_empty": not group_alive()}
            if not result["sidecar_cleanup"]["group_empty"]:
                result["status"] = "failed"
            result["gateway_exit"] = child.returncode
            if child.returncode != 0:
                result["status"] = "failed"
        (root / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps({"status": result["status"], "result": str(root / "result.json"), "error": result.get("error")}))
    return int(result["status"] != "passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(run(args.fixture, args.output_dir))
