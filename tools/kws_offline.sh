#!/system/bin/sh
# Offline keyword-spotter harness (R1j/R2-BM5 research).
#
# usage: kws_offline.sh <keywords-file> <pcm-file> [threshold] [score] [trailing-blanks] [model-dir]
#
# Why the token pre-check: sherpa rejects the WHOLE keywords file when one
# token is unknown ("Encode keywords failed"), while the driver still prints
# ready — a typo therefore looks like "the phrase does not work". Every token
# is validated against tokens.txt before the driver starts; a bad file exits 3
# with the offending line.
cd /data/local/tmp/hm-kws || exit 1
KW=$1; PCM=$2; THR=${3:-0.07}; SCORE=${4:-4.0}; TB=${5:-0}; MODEL=${6:-model}
[ -f "$KW" ] || { echo "missing keywords file: $KW"; exit 2; }
[ -f "$PCM" ] || { echo "missing pcm file: $PCM"; exit 2; }
while IFS= read -r line; do
  case "$line" in ''|'#'*) continue;; esac
  toks=${line%%@*}
  for tok in $toks; do
    if ! grep -q "^$tok " "$MODEL/tokens.txt"; then
      echo "bad token '$tok' in '$line'"; exit 3
    fi
  done
done < "$KW"
echo "== kw=$(basename "$KW") pcm=$(basename "$PCM") thr=$THR score=$SCORE tb=$TB"
./ld-linux-aarch64.so.1 --library-path . ./hm-kws \
  --tokens "$MODEL/tokens.txt" \
  --encoder "$MODEL/encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx" \
  --decoder "$MODEL/decoder-epoch-13-avg-2-chunk-16-left-64.onnx" \
  --joiner "$MODEL/joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx" \
  --keywords-file "$KW" --threshold "$THR" --score "$SCORE" --trailing-blanks "$TB" \
  --cpu 1 --name dot2-d --pace < "$PCM" 2>&1 | grep -E 'HIT|Cannot|failed|ready'
