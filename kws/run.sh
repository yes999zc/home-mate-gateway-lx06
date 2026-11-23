#!/system/bin/sh
# hm-kws launcher (D/T4).
#
# emOS has no /lib/ld-linux-aarch64.so.1, so the glibc-built driver runs under
# the bundled Debian loader (the KWS spike proved this path on 2026-11-10).
# Everything resolves next to this script; the firmware spawns it with KWS_CPU
# (default cpu3) and feeds raw s16le 16 kHz mono blocks on stdin.
#
# Sensitivity (2026-11-15, user report: dot2-e false-woke three times in 15 min
# while a TV talked). RUI's production point was --threshold 0.07 --score 4.0,
# which is maximally permissive (it was tuned for a 2-syllable wake word on a
# different model). sherpa-onnx's own defaults are 0.25/1.0; this bundle runs
# 0.25/3.0 — the standard operating point — and both are env-overridable so a
# node can be re-tuned from the firmware without a new bundle:
#   KWS_THRESHOLD=0.18 KWS_SCORE=3.5 -> more recall, more false wakes
# The ASR wake fallback stays on, so a stricter spotter cannot make a node
# deaf: the word still wakes through the transcript (~1 s slower).
#
# `--name` is only the label the driver puts in its `ready`/`hit` events. It is
# read from the node's identity file instead of being baked in here, so this
# script is IDENTICAL on every node and can ship inside a signed release
# (2026-11-15: the seed used to patch `--name dot2-d` per device, which made the
# bundle node-specific and therefore unshippable as a component).
cd /data/local/tmp/hm-kws || exit 1
name=unknown
if [ -f /data/local/tmp/hm-direct/device-id ]; then
    name=$(cat /data/local/tmp/hm-direct/device-id)
fi
set -- \
  --tokens model/tokens.txt \
  --encoder model/encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
  --decoder model/decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
  --joiner model/joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
  --keywords-file kw.txt \
  --threshold "${KWS_THRESHOLD:-0.25}" --score "${KWS_SCORE:-3.0}" --trailing-blanks 0 \
  --l2-threshold "${KWS_L2_THRESHOLD:-0.0}" --l2-score "${KWS_L2_SCORE:-2.20}" \
  --min-wake-ms "${KWS_MIN_WAKE_MS:-400}" --max-wake-ms "${KWS_MAX_WAKE_MS:-1400}" \
  --max-token-gap-ms "${KWS_MAX_TOKEN_GAP_MS:-380}" --l2-min-tokens "${KWS_L2_MIN_TOKENS:-7}" \
  --cpu "${KWS_CPU:-3}" --name "$name"
# Dot2 runs Silero VAD inside hm-core (in-process tract-onnx). Only attach sidecar
# Silero VAD when KWS_VAD_MODEL is explicitly set (and != "off") to an existing file.
if [ -n "${KWS_VAD_MODEL:-}" ] && [ "${KWS_VAD_MODEL}" != "off" ] && [ -f "$KWS_VAD_MODEL" ]; then
    set -- "$@" --vad-model "$KWS_VAD_MODEL"
fi
exec ./ld-linux-aarch64.so.1 --library-path . ./hm-kws "$@"
