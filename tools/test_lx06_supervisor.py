"""Boot and switch the LX06 supervisor against disposable archives and stub services."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


SUPERVISOR = Path(__file__).resolve().parents[1] / "deploy/lx06/init.sh"
STUB = b'#!/bin/sh\nif [ "$1" = lx06-wake-contract ]; then echo lx06-idle-wake-v1; exit; fi\nprintf "%s %s %s\\n" "${0##*/}" "${HM_LX06_DUAL_WAKE:-}" "$*" >> "$FIXTURE/starts"\n'


def archive(path, files):
    with tarfile.open(path, "w:xz") as out:
        for name, data in files.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            entry.mode = 0o755
            out.addfile(entry, io.BytesIO(data))


class TestSupervisor(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.flash = self.root / "flash"
        self.flash.mkdir()
        self.vendor = self.root / "native-journal"
        self.vendor.mkdir()
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": STUB, "silero_vad.onnx": b"fixture",
        })
        archive(self.root / "kws.tar.xz", {
            name: STUB for name in ("run.sh", "hm-kws", "ld-linux-aarch64.so.1")
        })
        (self.flash / "kws.sha256").write_text(hashlib.sha256((self.root / "kws.tar.xz").read_bytes()).hexdigest())
        (self.flash / "kws.url").write_text((self.root / "kws.tar.xz").as_uri())
        unxz = self.flash / "unxz"
        unxz.write_text('#!/bin/sh\necho unpack >> "$FIXTURE/count"\nexec xz -dc\n')
        unxz.chmod(0o755)
        # Exercise the shipped function bodies. Replace just the boot invocation
        # and board service/process discovery at the host fixture boundary.
        body, launch, tail = SUPERVISOR.read_text().rpartition("\nrun_supervisor &")
        assert launch and not tail.strip()
        self.body = body + """
PATH="$HOST_BIN_PATH"
HM_FLASH_DIR="$FIXTURE/flash"
HM_STATE_DIR="$HM_FLASH_DIR/state"
HM_RUN_DIR="$FIXTURE/hm"
KWS_DIR="$FIXTURE/hm-kws"
NATIVE_MUTE="$FIXTURE/mute"
NATIVE_LOG_DIR="$FIXTURE/native-journal"
WD_LOG="$FIXTURE/wd"
CORE_LOG="$FIXTURE/core"
GW_LOG="$FIXTURE/gateway"
start_native_services() { echo "native $*" >> "$FIXTURE/services"; }
stop_runtime() { echo "stop $*" >> "$FIXTURE/services"; }
runtime_pid() { return 1; }
pidof() { [ "$1" = mipns-xiaomi ] && [ -f "$FIXTURE/pns-live" ]; }
pns_service() {
    echo "pns $1" >> "$FIXTURE/services"
    if [ "$1" = start ]; then touch "$FIXTURE/pns-live"; else rm -f "$FIXTURE/pns-live"; fi
}
last_mode=""
init_state_if_needed
"""

    def run_shell(self, steps):
        return subprocess.run(
            ["sh", "-eu"], input=self.body + steps, text=True,
            env={**os.environ, "FIXTURE": str(self.root), "HOST_BIN_PATH": os.environ["PATH"]}, capture_output=True,
            timeout=15,
        )

    def test_cold_dual_pin_and_return_reuse_cache(self):
        r = self.run_shell("""
supervise_tick
wait
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
printf 'homemate\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
""")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"] * 2)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(sum(s.startswith("hm-gateway 1 firmware ") for s in starts), 2)
        self.assertEqual(sum("lx06-switch-watch" in s for s in starts), 1)
        self.assertIn("pns start", (self.root / "services").read_text().splitlines())
        self.assertTrue((self.flash / "state/models/silero_vad.onnx").is_file())

    def test_cold_start_enables_the_bundled_gateway_vad_model(self):
        gateway = STUB + b'printf "%s" "${KWS_VAD_MODEL:-}" > "$FIXTURE/kws-vad-model"\n'
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": gateway, "silero_vad.onnx": b"fixture",
        })
        result = self.run_shell("unset KWS_VAD_MODEL; supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "kws-vad-model").read_text(), str(self.root / "hm/silero_vad.onnx"))

    def test_first_home_wake_has_a_journal_before_any_native_instruction(self):
        result = self.run_shell("""
rmdir "$NATIVE_LOG_DIR"
supervise_tick
wait
grep -q "^hm-gateway 1 firmware " "$FIXTURE/starts"
test ! -e "$NATIVE_LOG_DIR/instruction.log"
mkdir "$NATIVE_LOG_DIR"
supervise_tick
wait
test -f "$NATIVE_LOG_DIR/instruction.log"
test ! -s "$NATIVE_LOG_DIR/instruction.log"
""")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())

    def test_start_preserves_vendor_instructions(self):
        instruction = self.vendor / "instruction.log"
        instruction.write_bytes(b"active dialog")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(instruction.read_bytes(), b"active dialog")

    def test_wrong_keyword_archive_never_starts_gateway(self):
        (self.flash / "kws.sha256").write_text("0" * 64)
        result = self.run_shell("if supervise_tick; then exit 9; fi; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "hm-kws/run.sh").exists())
        self.assertNotIn(" 1 firmware ", (self.root / "starts").read_text())

    def test_legacy_native_selection_starts_dual_but_explicit_pin_persists(self):
        (self.flash / "mode").write_text("xiaoai\n")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())
        (self.flash / "mode").write_text("xiaoai-pinned\n")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "starts").read_text().splitlines()[-1].startswith(
            "hm-gateway  lx06-switch-watch "
        ))

    def test_old_cached_gateway_is_replaced_before_dual_start(self):
        cache = self.root / "hm"
        cache.mkdir()
        for name in ("hm-core", "hm-gateway"):
            binary = cache / name
            binary.write_text("#!/bin/sh\nexit 2\n")
            binary.chmod(0o755)
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"] * 2)

    def test_old_flash_gateway_does_not_take_exclusive_audio(self):
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": b"#!/bin/sh\nexit 2\n",
        })
        result = self.run_shell("if supervise_tick; then exit 9; fi")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "hm/hm-gateway").exists())
        self.assertFalse((self.root / "starts").exists())
        self.assertTrue((self.root / "pns-live").exists())

    def test_bad_download_retries_without_replacing_main_bundle(self):
        (self.flash / "kws.url").write_text((self.root / "absent").as_uri())
        r = self.run_shell("""
if supervise_tick; then exit 9; fi
wait
test ! -e "$KWS_DIR/run.sh"
printf '%s' "$RECOVER_URL" > "$HM_FLASH_DIR/kws.url"
supervise_tick
wait
""".replace("$RECOVER_URL", (self.root / "kws.tar.xz").as_uri()))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.root / "starts").read_text().count(" 1 firmware "), 1)

    def test_pinned_native_honors_management_mute_even_with_broken_bundle(self):
        (self.flash / "hm-bundle.tar.xz").write_bytes(b"truncated")
        result = self.run_shell("""
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
printf '{"volume":40, "muted":true}' > "$HM_STATE_DIR/state.json"
touch "$FIXTURE/pns-live"
if supervise_tick; then exit 9; fi
test ! -e "$FIXTURE/pns-live"
printf '{"muted":false,"volume":40}' > "$HM_STATE_DIR/state.json"
if supervise_tick; then exit 9; fi
test -e "$FIXTURE/pns-live"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_wake_reads_journal_with_lx06_busybox_tr(self):
        # This ROM builds tr without character classes. Its -d operand then
        # deletes the literal eight bytes in '[:space:]', including the colon.
        result = self.run_shell(r"""
tr() {
    if [ "$1" = -d ] && [ "$2" = '[:space:]' ]; then
        command tr -d '\133\072\163\160\141\143\145\135'
    else
        command tr "$@"
    fi
}
journal="$HM_STATE_DIR/state.json"
printf '%s' '{"muted":false,"volume":92}' > "$journal"
restore_native_wake
test -e "$FIXTURE/pns-live"
printf '{\r\n\t"volume" : 92,  "muted" : true\r\n}' > "$journal"
restore_native_wake
test ! -e "$FIXTURE/pns-live"
printf '{\r\n\t"volume" : 92,  "muted" : false\r\n}' > "$journal"
restore_native_wake
test -e "$FIXTURE/pns-live"
printf '%s' '{"muted":f alse,"volume":92}' > "$journal"
restore_native_wake
test ! -e "$FIXTURE/pns-live"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bad_main_archive_never_starts_partial_binary(self):
        (self.flash / "hm-bundle.tar.xz").write_bytes(b"truncated")
        r = self.run_shell("if supervise_tick; then exit 9; fi\n")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse((self.root / "hm/hm-core").exists())
        self.assertFalse((self.root / "starts").exists())

    def test_start_native_services_skips_live_ledserver_and_dismisses_boot_blue_ring(self):
        body, launch, tail = SUPERVISOR.read_text().rpartition("\nrun_supervisor &")
        assert launch and not tail.strip()
        init_d = self.root / "init.d"
        init_d.mkdir()
        for svc in ("led", "touchpad", "mico_aivs_lab"):
            script = init_d / svc
            script.write_text(f'#!/bin/sh\necho "{svc} $*" >> "$FIXTURE/native.log"\n')
            script.chmod(0o755)
        shut_led = self.root / "shut_led"
        shut_led.write_text('#!/bin/sh\necho "shut_led $*" >> "$FIXTURE/native.log"\n')
        shut_led.chmod(0o755)

        script = body + """
PATH="$HOST_BIN_PATH"
INIT_D_DIR="$FIXTURE/init.d"
SHUT_LED_BIN="$FIXTURE/shut_led"
WD_LOG="$FIXTURE/wd"
pidof() { [ "$1" = ledserver ] && [ -f "$FIXTURE/led-live" ]; }

# 1) When ledserver is already running (redeploy/mode-switch), do NOT re-run
#    `/etc/init.d/led start` (which fires `show_led 4`), and dismiss stale L=4.
touch "$FIXTURE/led-live"
start_native_services

# 2) When ledserver is stopped, start it and immediately dismiss boot L=4.
rm -f "$FIXTURE/led-live"
start_native_services
"""
        r = subprocess.run(
            ["sh", "-eu"], input=script, text=True,
            env={**os.environ, "FIXTURE": str(self.root), "HOST_BIN_PATH": os.environ["PATH"]},
            capture_output=True, timeout=15,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(
            (self.root / "native.log").read_text().splitlines(),
            [
                "shut_led 4",
                "touchpad start",
                "mico_aivs_lab start",
                "led start",
                "shut_led 4",
                "touchpad start",
                "mico_aivs_lab start",
            ],
        )


if __name__ == "__main__":
    unittest.main()
