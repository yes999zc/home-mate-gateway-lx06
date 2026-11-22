#!/usr/bin/env bash
# Build and pack the Scheme A compressed runtime bundles for Xiaomi AI Speaker Pro (LX06).
#
# Scheme A storage topology:
#   - /data/hm/hm-bundle.tar.xz (<= 6.0 MiB):
#       - hm-core (aarch64-unknown-linux-musl, --slim / --no-default-features)
#       - hm-gateway (aarch64-unknown-linux-musl, static musl + rust-lld)
#       - silero_vad.onnx (pinned SHA-256: 9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6)
#       - web/ (compiled console assets from hm-core web/dist)
#   - /data/hm/hm-bundle.sha256
#   - /data/hm/unxz (static aarch64 xz decompressor)
#   - /data/hm/kws.sha256 + hm-kws-lx06.tar.xz (unpacked to /tmp/hm-kws via kws.url)
#       - hm-kws (aarch64 glibc sidecar with #63/#65/#66 Silero VAD fixes)
#       - deploy/lx06/kws-run.sh -> run.sh (--vad-model /tmp/hm/silero_vad.onnx --vad-gain 3.0)
#       - Sherpa-ONNX KWS model + runtime shared libraries
set -euo pipefail

HERE=$(cd "$(dirname "$0")/.." && pwd)
CORE_REPO="${CORE_REPO:-$(cd "$HERE/../home mate b" && pwd)}"
OUT_DIR="${OUT_DIR:-$HERE/build/lx06}"
VAD_MODEL="${VAD_MODEL:-/Users/bakeyzhang/AI_Workspace/03_Data/Models/sherpa-onnx/silero_vad.onnx}"
PINNED_VAD_SHA256="9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"
MAX_MAIN_BUNDLE_BYTES=$((6 * 1024 * 1024)) # 6.0 MiB hard gate for 13.3 MiB /data UBIFS

SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        *)
            echo "pack_lx06_bundle: unknown option '$arg' (supported: --skip-build)" >&2
            exit 2
            ;;
    esac
done

if [[ "$SKIP_BUILD" -eq 0 ]]; then
    echo "=== [1/4] Building slim hm-core (--no-default-features, aarch64-unknown-linux-musl) ==="
    (cd "$CORE_REPO" && ./tools/build_core.sh --slim)
    if [[ ! -f "$CORE_REPO/web/dist/index.html" ]]; then
        echo "=== [1b/4] Building web console (npm run build) ==="
        (cd "$CORE_REPO/web" && npm run build)
    fi

    echo "=== [2/4] Building LX06 hm-gateway (aarch64-unknown-linux-musl) ==="
    (cd "$HERE" && ./tools/build_firmware_lx06.sh)

    if [[ -d "${SHERPA_PKG:-/private/tmp/sherpa-pkg}" ]]; then
        echo "=== [2b/4] Rebuilding aarch64 hm-kws sidecar ==="
        SHERPA_PKG="${SHERPA_PKG:-/private/tmp/sherpa-pkg}" "$HERE/kws/build.sh"
    fi
fi

CORE_BIN="$CORE_REPO/target/aarch64-unknown-linux-musl/release/hm-core"
GW_BIN="$HERE/target/aarch64-unknown-linux-musl/release/home-mate-device-gateway"
WEB_DIST="$CORE_REPO/web/dist"
KWS_BIN="$HERE/kws/hm-kws"
KWS_RUN="$HERE/deploy/lx06/kws-run.sh"

for f in "$CORE_BIN" "$GW_BIN" "$VAD_MODEL" "$WEB_DIST/index.html" "$KWS_BIN" "$KWS_RUN"; do
    if [[ ! -s "$f" ]]; then
        echo "pack_lx06_bundle: required input missing or empty: $f" >&2
        exit 1
    fi
done

mkdir -p "$OUT_DIR"

echo "=== [3/4] Packing main bundle: $OUT_DIR/hm-bundle.tar.xz ==="
python3 - "$CORE_BIN" "$GW_BIN" "$VAD_MODEL" "$WEB_DIST" "$OUT_DIR" "$PINNED_VAD_SHA256" "$MAX_MAIN_BUNDLE_BYTES" <<'PY'
import hashlib
import io
import lzma
import os
from pathlib import Path
import sys
import tarfile

core_bin = Path(sys.argv[1])
gw_bin = Path(sys.argv[2])
vad_model = Path(sys.argv[3])
web_dist = Path(sys.argv[4])
out_dir = Path(sys.argv[5])
pinned_vad_sha = sys.argv[6]
max_bytes = int(sys.argv[7])

def sha256_file(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()

vad_sha = sha256_file(vad_model)
if vad_sha != pinned_vad_sha:
    sys.exit(f"VAD model SHA-256 mismatch: got {vad_sha}, expected {pinned_vad_sha}")

# Ensure slim hm-core does NOT carry the fat in-process tract-onnx marker
core_bytes = core_bin.read_bytes()
if b"hm-core-vad:silero" in core_bytes:
    sys.exit("hm-core contains fat 'hm-core-vad:silero' marker; LX06 requires --slim build!")

bundle_path = out_dir / "hm-bundle.tar.xz"
stage_path = out_dir / "hm-bundle.tar.xz.part"

def add_entry(tf: tarfile.TarFile, arcname: str, data: bytes | None, mode: int, is_dir: bool = False):
    info = tarfile.TarInfo(arcname)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = 1700000000
    if is_dir:
        info.type = tarfile.DIRTYPE
        info.mode = mode
        tf.addfile(info)
    else:
        assert data is not None
        info.type = tarfile.REGTYPE
        info.mode = mode
        info.size = len(data)
        tf.addfile(info, io.BytesIO(data))

raw_tar = io.BytesIO()
with tarfile.open(fileobj=raw_tar, mode="w", format=tarfile.PAX_FORMAT) as tf:
    add_entry(tf, "hm-core", core_bytes, 0o755)
    add_entry(tf, "hm-gateway", gw_bin.read_bytes(), 0o755)
    add_entry(tf, "silero_vad.onnx", vad_model.read_bytes(), 0o644)
    add_entry(tf, "web", None, 0o755, is_dir=True)
    for root, dirs, files in os.walk(web_dist):
        dirs.sort()
        files.sort()
        rel_root = Path(root).relative_to(web_dist)
        for d in dirs:
            arc = (Path("web") / rel_root / d).as_posix()
            add_entry(tf, arc, None, 0o755, is_dir=True)
        for f in files:
            p = Path(root) / f
            arc = (Path("web") / rel_root / f).as_posix()
            add_entry(tf, arc, p.read_bytes(), 0o644)

# Compress with ARM64 BCJ pre-filter + LZMA2 preset=9e (8 MiB dictionary) + CRC64
# Verified compatible with on-device static /data/hm/unxz (xz-embedded with XZ_DEC_BCJ / BCJ_ARM64).
import subprocess
compressed = subprocess.run(
    ["xz", "--arm64", "--lzma2=preset=9e,dict=8MiB", "--check=crc64", "-c"],
    input=raw_tar.getvalue(),
    capture_output=True,
    check=True,
).stdout
stage_path.write_bytes(compressed)
stage_path.replace(bundle_path)

bundle_size = bundle_path.stat().st_size
bundle_sha = hashlib.sha256(compressed).hexdigest()
(out_dir / "hm-bundle.sha256").write_text(f"{bundle_sha}\n")

if bundle_size > max_bytes:
    sys.exit(f"FAIL: hm-bundle.tar.xz is {bundle_size} bytes (> {max_bytes} byte /data cap)")

print(f"PASS  hm-core (slim)   : {len(core_bytes):>10,} B  sha256={hashlib.sha256(core_bytes).hexdigest()}")
print(f"PASS  hm-gateway (lx06): {gw_bin.stat().st_size:>10,} B  sha256={sha256_file(gw_bin)}")
print(f"PASS  silero_vad.onnx  : {vad_model.stat().st_size:>10,} B  sha256={vad_sha}")
print(f"PASS  hm-bundle.tar.xz : {bundle_size:>10,} B ({bundle_size * 100.0 / max_bytes:.1f}% of 6.0 MiB /data gate)")
print(f"PASS  hm-bundle.sha256 : {bundle_sha}")
PY

# Copy static aarch64 unxz and init.sh if available
if [[ -s /private/tmp/lx06-bundle/unxz ]]; then
    cp /private/tmp/lx06-bundle/unxz "$OUT_DIR/unxz"
    chmod 0755 "$OUT_DIR/unxz"
fi
cp "$HERE/deploy/lx06/init.sh" "$OUT_DIR/init.sh"
chmod 0755 "$OUT_DIR/init.sh"

# Pack hm-kws-lx06.tar.xz if base runtime directory or archive is available
KWS_BASE_DIR="${KWS_BASE_DIR:-/private/tmp/lx06-bundle/kws-extract}"
if [[ -d "$KWS_BASE_DIR" && -f "$KWS_BASE_DIR/ld-linux-aarch64.so.1" ]]; then
    echo "=== [4/4] Packing KWS sidecar bundle: $OUT_DIR/hm-kws-lx06.tar.xz ==="
    python3 - "$KWS_BASE_DIR" "$KWS_BIN" "$KWS_RUN" "$OUT_DIR" <<'PY'
import hashlib
import io
import lzma
import os
from pathlib import Path
import sys
import tarfile

base_dir = Path(sys.argv[1])
kws_bin = Path(sys.argv[2])
kws_run = Path(sys.argv[3])
out_dir = Path(sys.argv[4])

EXECUTABLE_NAMES = {
    "run.sh",
    "hm-kws",
    "ld-linux-aarch64.so.1",
    "libc.so.6",
    "libsherpa-onnx-c-api.so",
}

raw_tar = io.BytesIO()
with tarfile.open(fileobj=raw_tar, mode="w", format=tarfile.PAX_FORMAT) as tf:
    for root, dirs, files in os.walk(base_dir):
        dirs.sort()
        files.sort()
        rel_root = Path(root).relative_to(base_dir)
        for d in dirs:
            arc = (rel_root / d).as_posix()
            info = tarfile.TarInfo(arc)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 1700000000
            tf.addfile(info)
        for f in files:
            p = Path(root) / f
            arc = (rel_root / f).as_posix()
            if arc == "hm-kws":
                data = kws_bin.read_bytes()
            elif arc == "run.sh":
                data = kws_run.read_bytes()
            else:
                data = p.read_bytes()
            info = tarfile.TarInfo(arc)
            info.type = tarfile.REGTYPE
            info.mode = 0o755 if f in EXECUTABLE_NAMES else 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 1700000000
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))

import subprocess
compressed = subprocess.run(
    ["xz", "--arm64", "--lzma2=preset=9e,dict=8MiB", "--check=crc64", "-c"],
    input=raw_tar.getvalue(),
    capture_output=True,
    check=True,
).stdout
kws_tar_path = out_dir / "hm-kws-lx06.tar.xz"
kws_tar_path.write_bytes(compressed)
kws_sha = hashlib.sha256(compressed).hexdigest()
(out_dir / "kws.sha256").write_text(f"{kws_sha}\n")
print(f"PASS  hm-kws (aarch64) : {kws_bin.stat().st_size:>10,} B  sha256={hashlib.sha256(kws_bin.read_bytes()).hexdigest()}")
print(f"PASS  hm-kws-lx06.tar.xz: {len(compressed):>9,} B  sha256={kws_sha}")
PY
else
    echo "=== [4/4] Skipping hm-kws-lx06.tar.xz repack (KWS_BASE_DIR not found at $KWS_BASE_DIR) ==="
fi

echo "=== LX06 Scheme A bundle artifacts ready in $OUT_DIR ==="
ls -lh "$OUT_DIR"
