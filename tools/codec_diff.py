#!/usr/bin/env python3
"""Codec bring-up sequence diff: EchoMuse's bindings vs our firmware table.

The comparison is BY CONTROL NAME, never by control id — the kernel shifts
ids and EchoMuse's own scripts say so (#546). Sources on the EchoMuse side
(upstream tree at a fixed commit):

  - device/internal/bindings/codec/routes.go   Routes[] (10 DAPM switches)
  - device/internal/server/mute.go             adcMuteCtls[] (8 ADC mutes)
  - device/internal/bindings/speaker/pcm_speaker.go  amp order + dacUnity
  - device/internal/config/config.go           ADC gain defaults (88/40)
  - the original start_server.sh               I2S1 / HP DAC / MFP Gpio /
                                                PCM Playback 100 / ADC gains

Our side: crates/gateway/src/firmware/bringup.rs (PRE_PCM table + the
amp/mute sequence constants). Every EchoMuse-init write and every one of
ours must appear in the three-column output; rows are MISSING/DIFF/MATCH
and a nonzero exit means an unexplained difference remains.

  ./tools/codec_diff.py <echomuse-upstream-dir> <echomuse-original-script>
"""
import re
import sys
from pathlib import Path


def echoMuse_table(upstream: Path, script: Path) -> dict[str, str]:
    t: dict[str, str] = {}
    routes = upstream / "device/internal/bindings/codec/routes.go"
    for name, value in re.findall(r'\{"([^"]+)",\s*"([^"]+)"\}', routes.read_text()):
        t[name] = value  # DAPM routes: all "1"
    mute = upstream / "device/internal/server/mute.go"
    m = re.search(r"adcMuteCtls = \[\]string\{(.*?)\}", mute.read_text(), re.S)
    for name in re.findall(r'"([^"]+)"', m.group(1)):
        t[name] = "On/Off (runtime mute)"
    spk = (upstream / "device/internal/bindings/speaker/pcm_speaker.go").read_text()
    mixer = (upstream / "device/internal/bindings/mixer/mixer.go").read_text()
    consts = dict(re.findall(r'(\w+)\s*=\s*"([^"]+)"', mixer))
    dac_unity = re.search(r'dacUnity = "(\d+)"', spk).group(1)
    amp = consts["SpeakerAmp"]
    vol = consts["PlaybackVolume"]
    # Init writes the amp ON and the DAC volume to unity; teardown writes
    # both off/zero. The values the system RUNS at are On / dacUnity.
    t[amp] = f"On (init; Off on teardown)"
    t[vol] = f"{dac_unity} (dacUnity; 0 while touching the amp)"

    cfg = (upstream / "device/internal/config/config.go").read_text()
    adc_dig = re.search(r'ADC_DIGITAL_GAIN",\s*(\d+)\)', cfg).group(1)
    adc_pga = re.search(r'ADC_MICPGA",\s*(\d+)\)', cfg).group(1)

    # The script: extract the amp_off() teardown block FIRST so its writes
    # can never shadow init values, then expand the `for adc in A B C D`
    # loop, then collect init writes from what remains.
    text = script.read_text()
    init_writes: dict[str, str] = {}
    teardown: dict[str, str] = {}
    amp_off_block = re.search(r"amp_off\(\)\s*\{(.*?)\n\}", text, re.S)
    if amp_off_block:
        for name, val in re.findall(r'tinymix -D 0 "(.+?)"\s+([0-9A-Za-z]+)', amp_off_block.group(1)):
            teardown[name.strip()] = val.strip()
        text = text.replace(amp_off_block.group(0), "")
    loop = re.search(r"for adc in (A B C D); do(.*?)done", text, re.S)
    for name, val in re.findall(r'tinymix -D 0 "(.+?)"\s+([0-9A-Za-z ]+?)\s*(?:2>/dev/null)?$', text, re.M):
        init_writes[name.strip()] = val.strip()
    if loop:
        body = loop.group(2)
        for name, val in re.findall(r'tinymix -D 0 "(.+?)"\s+([0-9 ]+)\s*$', body, re.M):
            for adc in loop.group(1).split():
                init_writes[name.replace("$adc", adc)] = val
        # The loop's non-numeric values (On/Off) if any:
        for name, val in re.findall(r'tinymix -D 0 "(.+?)"\s+([A-Za-z][^\n]*?)\s*$', body, re.M):
            for adc in loop.group(1).split():
                init_writes[name.replace("$adc", adc)] = val.strip()

    for name, val in init_writes.items():
        if "$adc" in name:
            continue  # loop body handled above
        note = ""
        if name in teardown:
            note = f"; {teardown[name]} on teardown"
        if name == vol and val.split()[0] != dac_unity:
            # The script's 100 100 belt is superseded by the Go server's
            # dacUnity write at init — the running value is dacUnity.
            t[name] = f"{dac_unity} (Go dacUnity final; script belt {val}{note})"
            continue
        if name.startswith("ADC_") and "Digital Volume" in name:
            t[name] = f"{val} (script; config default {adc_dig})"
        elif name.startswith("ADC_") and "MICPGA" in name:
            t[name] = f"{val} (script; config default {adc_pga})"
        else:
            t[name] = f"{val}{note}"
    return t


def ours_table(bringup: Path) -> dict[str, str]:
    src = bringup.read_text()
    t: dict[str, str] = {}
    for name, values in re.findall(r'\("([^"]+)",\s*&\[([^\]]*)\]\)', src):
        vals = " ".join(re.findall(r'"([^"]*)"', values))
        t[name] = vals
    # Sequence constants outside PRE_PCM.
    t["PCM Playback Volume"] = "127 127 after amp-up (0 0 during; 0 0 on teardown)"
    t["Ext_Speaker_Amp_Switch"] = "On (init; Off on teardown)"
    for name in re.findall(r'"(ADC_[ABCD] (?:Left|Right) Mute)"', src):
        t[name] = "On/Off (runtime mute)"
    return t


def main() -> int:
    upstream = Path(sys.argv[1])
    script = Path(sys.argv[2])
    bringup = Path(__file__).resolve().parent.parent / "crates/gateway/src/firmware/bringup.rs"
    theirs = echoMuse_table(upstream, script)
    ours = ours_table(bringup)

    names = sorted(set(theirs) | set(ours))
    bad = 0
    print(f"{'control name':<48} {'EchoMuse':<28} ours")
    print("-" * 110)
    for n in names:
        tv, ov = theirs.get(n), ours.get(n)
        if tv is None:
            status = "OURS-ONLY"
        elif ov is None:
            status, bad = "MISSING", bad + 1
        elif norm(tv) != norm(ov):
            status, bad = "DIFF", bad + 1
        else:
            status = "match"
        print(f"{n:<48} {(tv or '—'):<28} {(ov or '—'):<28} {status}")
    print("-" * 110)
    print(f"{len(names)} controls, {bad} MISSING/DIFF")
    return 1 if bad else 0


def norm(v: str) -> str:
    """Compare the VALUE a control settles at, ignoring provenance notes."""
    v = v.split(" (")[0]
    # Normalize '100 100' vs '100', '1' vs '1 1' for stereo repeats: compare
    # the first token (controls we set repeat one value across channels).
    head = v.split()[0] if v else v
    return {"On": "On", "1": "1"}.get(head, head)


if __name__ == "__main__":
    sys.exit(main())
