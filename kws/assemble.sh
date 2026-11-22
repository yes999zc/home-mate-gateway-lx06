#!/usr/bin/env bash
# Assemble the COMPLETE hm-kws runtime bundle — the thing that ships.
#
# The driver alone is a 31 KB ELF; what the device actually runs is the driver
# plus the Debian loader, its glibc/std C++ runtime, the sherpa-onnx runtime and
# the model. Until 2026-11-15 that bundle only existed as a hand-made directory
# on the dev machine (`~/.home-mate/kws-build/bundle`), which is why the keyword
# spotter could not travel with a release. This script is the reproducible way
# to produce it.
#
# Inputs:
#   --sherpa <dir>   unpacked sherpa-onnx-v<ver>-linux-aarch64-shared-cpu
#   --model  <dir>   streaming zipformer export (encoder/decoder/joiner/tokens.txt)
#   --glibc  <dir>   Debian arm64 runtime: ld-linux-aarch64.so.1, libc.so.6,
#                    libstdc++.so.6, libgcc_s.so.1 … (unpacked libc6 /
#                    libstdc++6 / libgcc-s1 wheels; the driver is built for
#                    aarch64-linux-gnu, and emOS ships only bionic)
#   --driver <file>  built hm-kws (kws/build.sh)
#   --out    <dir>   bundle directory (created/cleaned)
#
# The bundle never contains `kw.txt`: the word list is the NODE's configuration
# and is written by the gateway on `kws_deploy`. Shipping one would silently
# overwrite a household's vocabulary on the next release.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
sherpa=""
model=""
glibc=""
driver="$here/hm-kws"
out=""

while [ $# -gt 0 ]; do
    case "$1" in
        --sherpa) sherpa=$2; shift 2 ;;
        --model) model=$2; shift 2 ;;
        --glibc) glibc=$2; shift 2 ;;
        --driver) driver=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "assemble: unknown argument $1" >&2; exit 2 ;;
    esac
done

[ -n "$sherpa" ] && [ -d "$sherpa" ] || { echo "assemble: --sherpa <dir> is required" >&2; exit 2; }
[ -n "$model" ] && [ -d "$model" ] || { echo "assemble: --model <dir> is required" >&2; exit 2; }
[ -n "$glibc" ] && [ -d "$glibc" ] || { echo "assemble: --glibc <dir> is required" >&2; exit 2; }
[ -f "$driver" ] || { echo "assemble: driver not found at $driver (run build.sh)" >&2; exit 2; }
[ -n "$out" ] || { echo "assemble: --out <dir> is required" >&2; exit 2; }

for f in libsherpa-onnx-c-api.so libonnxruntime.so; do
    [ -f "$sherpa/lib/$f" ] || { echo "assemble: $sherpa/lib/$f missing" >&2; exit 1; }
done
for f in encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
         decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
         joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx tokens.txt; do
    [ -f "$model/$f" ] || { echo "assemble: $model/$f missing" >&2; exit 1; }
done

rm -rf "$out"
mkdir -p "$out/model"
cp "$driver" "$out/hm-kws"
cp "$sherpa/lib/libsherpa-onnx-c-api.so" "$sherpa/lib/libonnxruntime.so" "$out/"
# The Debian loader + the glibc/std C++ runtime the driver needs on emOS
# (there is no /lib/ld-linux-aarch64.so.1 there). Taken from the sherpa package
# so the bundle is self-consistent: one source, one version.
# The loader and the C/C++ runtime come from the Debian arm64 packages, NOT
# from the sherpa bundle: the driver is linked against Debian glibc and emOS
# carries bionic only. `find` is used because each .deb unpacks into a
# different prefix (lib/, lib/aarch64-linux-gnu/, usr/lib/aarch64-linux-gnu/).
pick() {  # $1 filename in the glibc tree
    local hit
    # No `-type f`: inside the unpacked .debs some of these are symlinks
    # (libstdc++.so.6 -> libstdc++.so.6.0.30). cp dereferences them.
    hit=$(find "$glibc" -name "$1" 2>/dev/null | head -1)
    [ -n "$hit" ] || { echo "assemble: $1 not found under $glibc" >&2; exit 1; }
    echo "$hit"
}
cp "$(pick ld-linux-aarch64.so.1)" "$out/"
for lib in libc.so.6 libm.so.6 libstdc++.so.6 libgcc_s.so.1 libpthread.so.0 libdl.so.2 librt.so.1; do
    cp "$(pick "$lib")" "$out/"
done
# Exactly these four: the model directory also holds chunk-8 and non-int8
# variants that the launcher never opens, and shipping 40 MB of unused weights
# inside every release is not free (each release keeps its own copy).
for f in encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
         decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
         joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx tokens.txt; do
    cp "$model/$f" "$out/model/$f"
done
cp "$here/run.sh" "$out/run.sh"
chmod 755 "$out/hm-kws" "$out/ld-linux-aarch64.so.1" "$out/run.sh"
chmod 644 "$out"/*.so* "$out/model"/*

# A seed/release bundle must not carry a word list (see the header).
if [ -f "$out/kw.txt" ]; then
    echo "assemble: refusing to ship kw.txt (node configuration)" >&2
    exit 1
fi

echo "bundle: $out"
find "$out" -type f | sort | while read -r f; do
    printf '%10d  %s\n' "$(wc -c < "$f")" "${f#"$out"/}"
done
echo "total: $(du -sk "$out" | awk '{print $1}') KiB"
