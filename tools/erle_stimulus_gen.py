# G6 ERLE stimulus generator (acceptance §0): ONE fixed 30s speech segment,
# synthesized once with the product TTS (Volcano seed-tts-2.0, same voice),
# archived with SHA256. Never regenerate after the baseline is recorded —
# a different signal is a different measurement, not a rerun.
# Usage: erle_stimulus_gen.py <env_local_path> [out.wav]
#   env_local must define VOLC_API_KEY=... (value is never printed).
#!/usr/bin/env python3
import base64
import hashlib
import json
import os
import struct
import sys
import urllib.request

URL = "https://openspeech.bytedance.com/api/v3/tts/unidirectional"
RESOURCE = "seed-tts-2.0"
VOICE = "zh_female_xiaohe_uranus_bigtts"
RATE = 16000

# ~192 chars of everyday-assistant Mandarin: mixed sentence lengths and
# phonetics so the stimulus exercises the band the canceller must chase.
# Sized at the measured 6.4 chars/s pace to land at ~30s.
TEXT = (
    "今天天气晴朗，气温二十六度，适合出门散步。下午三点有一个提醒，"
    "别忘了给花浇水。晚上的菜单是番茄炒蛋和紫菜汤，米饭已经下锅。"
    "最近空气质量不错，窗户可以开着通风。如果你想听音乐，随时叫我一声，"
    "我可以在客厅或者卧室播放你喜欢的歌曲。周末的安排已经准备好了，"
    "周六上午去公园骑车，下午参观新的展览，周日在家整理房间，"
    "看一部轻松的电影。"
)


def pcm_to_wav(pcm: bytes, rate: int, channels: int) -> bytes:
    data_len = len(pcm)
    hdr = b"RIFF" + struct.pack("<I", 36 + data_len) + b"WAVE"
    hdr += b"fmt " + struct.pack("<IHHIIHH", 16, 1, channels, rate,
                                 rate * channels * 2, channels * 2, 16)
    hdr += b"data" + struct.pack("<I", data_len)
    return hdr + pcm


def main() -> int:
    env_path = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else "erle-stimulus.wav"
    key = ""
    with open(env_path) as f:
        for line in f:
            if line.startswith("VOLC_API_KEY="):
                key = line.split("=", 1)[1].strip()
    if not key:
        print("VOLC_API_KEY not found in", env_path)
        return 1

    payload = json.dumps({
        "user": {"uid": "home-mate-b-erle-stimulus"},
        "req_params": {
            "text": TEXT,
            "speaker": VOICE,
            "audio_params": {"format": "pcm", "sample_rate": RATE},
        },
    }).encode()
    req = urllib.request.Request(URL, data=payload, headers={
        "Content-Type": "application/json",
        "X-Api-Key": key,
        "X-Api-Resource-Id": RESOURCE,
    })
    with urllib.request.urlopen(req, timeout=30) as resp:
        body = resp.read().decode()

    pcm = bytearray()
    for line in body.splitlines():
        line = line.strip()
        if not line:
            continue
        frame = json.loads(line)
        code = frame.get("code", 0)
        if code not in (0, 20000000):
            print(f"volc error code={code} message={frame.get('message')}")
            return 1
        if frame.get("data"):
            pcm.extend(base64.b64decode(frame["data"]))
    if not pcm:
        print("no audio frames returned")
        return 1

    wav = pcm_to_wav(bytes(pcm), RATE, 1)
    with open(out, "wb") as f:
        f.write(wav)
    sha = hashlib.sha256(wav).hexdigest()
    dur = len(pcm) / 2 / RATE
    print(f"wrote {out}: {len(wav)} bytes, {dur:.2f}s, 16kHz mono s16")
    print(f"SHA256 {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
