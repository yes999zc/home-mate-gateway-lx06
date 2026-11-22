#!/bin/sh
set -eu
cd "$(dirname "$0")"
name=lx06
[ ! -s /data/hm/state/device-id ] || name=$(cat /data/hm/state/device-id)
model=${KWS_VAD_MODEL:-/tmp/hm/silero_vad.onnx}
[ -s "$model" ] || { echo "[kws] missing LX06 VAD model: $model" >&2; exit 1; }
exec ./ld-linux-aarch64.so.1 --library-path . ./hm-kws \
    --tokens model/tokens.txt \
    --encoder model/encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
    --decoder model/decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
    --joiner model/joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
    --keywords-file kw.txt --threshold "${KWS_THRESHOLD:-0.25}" --score "${KWS_SCORE:-3.0}" \
    --trailing-blanks 0 --cpu "${KWS_CPU:-3}" --name "$name" --vad-model "$model" \
    --vad-gain 3.0 --vad-min-speech 0.001 --vad-min-silence 0.001
