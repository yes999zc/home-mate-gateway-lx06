#!/bin/sh
# Home Mate 1-Command Non-Invasive Installer for Xiaomi AI Speaker Pro (LX06)
#
# Storage Scheme A:
#   - Stock mtd4/mtd5 SquashFS partitions remain 100% untouched (zero brick risk).
#   - Uploads /data/hm/{hm-bundle.tar.xz, hm-bundle.sha256, init.sh, unxz, kws.url, kws.sha256}
#   - Hooks /data/init.sh (open-xiaoai boot hook) -> /data/hm/init.sh
#
# Usage:
#   ./deploy/lx06/install-lx06.sh --device-ip <LX06_IP> --kws-url <HTTP_URL_TO_HM_KWS_TAR_XZ> [--bundle-dir build/lx06]

set -eu

DEVICE_IP=""
KWS_URL=""
BUNDLE_DIR="build/lx06"
SSH_USER="root"

usage() {
    cat <<EOF
Usage: $0 --device-ip <LX06_IP> --kws-url <HTTP_URL_TO_HM_KWS_TAR_XZ> [--bundle-dir build/lx06]

Options:
  --device-ip <IP>     IPv4 address of your Xiaomi AI Speaker Pro (LX06) with open-xiaoai SSH enabled
  --kws-url <URL>      LAN HTTP URL serving hm-kws-lx06.tar.xz (e.g. http://192.168.1.10:18089/hm-kws-lx06.tar.xz)
  --bundle-dir <DIR>   Directory containing built LX06 artifacts (default: build/lx06)

To uninstall cleanly and return to 100% stock XiaoAi:
  ssh root@<LX06_IP> 'rm -rf /data/hm /data/init.sh && reboot'
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --device-ip) DEVICE_IP="$2"; shift 2 ;;
        --kws-url) KWS_URL="$2"; shift 2 ;;
        --bundle-dir) BUNDLE_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ -z "$DEVICE_IP" ] || [ -z "$KWS_URL" ]; then
    echo "Error: --device-ip and --kws-url are required." >&2
    usage >&2
    exit 2
fi

for f in hm-bundle.tar.xz hm-bundle.sha256 kws.sha256 init.sh unxz; do
    if [ ! -f "$BUNDLE_DIR/$f" ]; then
        echo "Error: missing artifact '$BUNDLE_DIR/$f'. Run ./tools/pack_lx06_bundle.sh first." >&2
        exit 1
    fi
done

echo "==> [1/4] Checking /data free space on $SSH_USER@$DEVICE_IP ..."
ssh "$SSH_USER@$DEVICE_IP" 'mkdir -p /data/hm/state /tmp/hm-voice-events && df -k /data /tmp'

echo "==> [2/4] Uploading Scheme A bundle artifacts to $SSH_USER@$DEVICE_IP:/data/hm/ ..."
scp -O \
    "$BUNDLE_DIR/hm-bundle.tar.xz" \
    "$BUNDLE_DIR/hm-bundle.sha256" \
    "$BUNDLE_DIR/kws.sha256" \
    "$BUNDLE_DIR/init.sh" \
    "$BUNDLE_DIR/unxz" \
    "$SSH_USER@$DEVICE_IP:/data/hm/"

echo "==> [3/4] Writing kws.url and /data/init.sh boot hook ..."
ssh "$SSH_USER@$DEVICE_IP" "
set -e
printf '%s\n' '$KWS_URL' > /data/hm/kws.url
chmod +x /data/hm/init.sh /data/hm/unxz
cat << 'HOOK' > /data/init.sh
#!/bin/sh
exec /data/hm/init.sh \"\$@\"
HOOK
chmod +x /data/init.sh
sync
"

echo "==> [4/4] Starting Home Mate supervisor on $DEVICE_IP ..."
ssh "$SSH_USER@$DEVICE_IP" '
for p in $(ps | grep "/data/hm/init.sh" | grep -v grep | awk "{print \$1}"); do
    kill -9 "$p" 2>/dev/null || true
done
killall hm-gateway hm-core hm-kws 2>/dev/null || true
/data/hm/init.sh </dev/null >/dev/null 2>&1 &
sleep 14
ps | grep -E "hm-core|hm-gateway|hm-kws" | grep -v grep || true
'

echo ""
echo "✓ Home Mate LX06 deployment complete!"
echo "  Web Console: http://$DEVICE_IP:8930"
echo "  10-Min Beginner Guide: https://mymate.homes/#guideline"
