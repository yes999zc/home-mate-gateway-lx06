# Home Mate on Xiaomi AI Speaker Pro (LX06) — Feasibility & Adaptation Design

Date: 2026-11-19  
Branch: `feat/lx06-research` (worktree from `home-mate-device-gateway-b` `main` @ `9175292`)  
Live Probe Target: `root@192.168.1.50` (`MiAiSoundbox-LX06`, MAC `8c:53:c3:2c:63:64`, ROM `1.94.13`, `boot_part=boot0`)  
Status: **Step 1–3 Complete & Verified Live on LX06 (100% Silent Mode)** (Dot2 untouched, Core untouched)

---

## 1. Architectural & Protocol Boundaries

Per `home mate b/AGENTS.md`, `home mate b/docs/device-bridge-boundary.md`, and `home mate b/docs/device-gateway-project.md`:

- **Core (`hm-core`)** remains a single modular monolith. No LX06 brand/hardware branches are added to `hm-core`.
- **Gateway (`home-mate-device-gateway`)** communicates with `hm-core` strictly via the versioned LDJSON protocol (`PROTOCOL_VERSION = 2`, `PROTOCOL_MIN_VERSION = 0`, `SUPPORTED_PROTOCOLS = &[0, 2]`) over `127.0.0.1:8932` (loopback).
- **Capabilities & Profile**: `HelloDevice` already abstracts `stable_id`, `model`, `firmware`, `capabilities`, and `audio_profile`. Inside `hm-gateway`, `crates/gateway/src/firmware/profile.rs` (`DeviceProfile`, `AecScheme`, `KeyMap`, `VolumeScale`) and the `Hardware` trait (`crates/gateway/src/firmware/mod.rs`) already isolate hardware-specific ports from the `north.rs` / `playback.rs` session layer.

---

## 2. Live Hardware & OS Baseline (`192.168.1.50`, Verified 2026-11-19)

| Item | Live Probe Result (`192.168.1.50`) | Architectural Impact |
|---|---|---|
| **Kernel & Arch** | `Linux LX06 4.9.61 #1 SMP PREEMPT Tue Jul 1 12:16:10 2025 aarch64 GNU/Linux` | **64-bit `aarch64` kernel!** Can natively run **64-bit static `aarch64-unknown-linux-musl` binaries** (same target as `hm-core`) as well as 32-bit `armv7` binaries |
| **CPU** | 4× ARM Cortex-A53 (`CPU part: 0xd03`, ARMv8-A with `fp asimd aes pmull sha1 sha2 crc32`), Serial `250b4300e25a41f7928e427504a989f3` | Hardware NEON + AES/SHA crypto acceleration available |
| **Userspace libc** | 32-bit glibc 2.25 (`/lib/libc-2.25.so`, `/lib/ld-linux-armhf.so.3`) on OpenWrt/procd (`ROM 1.94.13`) | Static musl binaries (`aarch64-unknown-linux-musl`) bypass 32-bit glibc completely and use native 64-bit kernel syscalls/ioctls |
| **Memory (`free -m`)** | **241 MB total**, 60 MB used, 69 MB free, 111 MB buff/cache, **172 MB available** (with all stock daemons running) | Stopping stock voice/cloud daemons yields **~200 MB available RAM**, sufficient for on-device `hm-gateway` + `hm-core` |
| **Rootfs (`/`, `mtd4`)** | `/dev/mtdblock4` (`system0`), `36.8M` SquashFS (`ro`) inside `0x02820000` (**40.125 MiB**) partition | `mtd5` (`system1`, `0x02800000` = 40.0 MiB) is the untouched A/B backup partition (`boot_part=boot0`) |
| **Data (`/data`, `mtd6`)** | `/dev/ubi0_0` (`ubifs` on `mtd6` `0x013e0000`), **13.3 MB total**, 4.4 MB used, **8.2 MB available** | Fits `hm-gateway` (~3.4 MB) + state/config/SQLite DB, **cannot** hold `hm-core` (24.7 MB) |
| **Temp (`/tmp`)** | `tmpfs`, **120.9 MB total**, 1.0 MB used, **119.9 MB available** | Backed by RAM |
| **LED Ring** | **AW20054** at `/sys/devices/i2c-0/0-003a/` (`led_rgb`, `led_fade`, `led_hwen`, `led_imax`, `led_position`), DT node `i2c@ff805000/leds-aw20054@3a`, **18 LEDs** | Format: write `"<index> 0xBBGGRR"` (`0..17`) to `led_rgb` |
| **Buttons & Aux-in** | `event0`: `adc_keypad` (`KEY=800 c0040 0 0 0` → `KEY_VOLUMEUP=115`, `KEY_VOLUMEDOWN=114`, `KEY_PLAYPAUSE=164`, `KEY_MICMUTE=248`); `event1`: `Xiaomi mico auxiliary port` (`SW=2000`) | 4 physical keys on `/dev/input/event0` + AUX jack switch on `/dev/input/event1` |

### Live Stock Daemon Conflict Inventory (`ps w`)

| Process | PID (live) | Resource Held / Conflict | Handling in Direct-Occupation Mode |
|---|---|---|---|
| `/usr/bin/mipns-xiaomi -c /usr/share/xiaomi/xaudio_engine.conf -r opus32 -l` | 1279 | Holds `/dev/snd/pcmC0D3c` (`hw:0,3`), `/dev/snd/pcmC0D2p` (`hw:0,2`), `/dev/snd/controlC0`, runs vendor AEC/KWS | **Stop** before starting `hm-gateway` |
| `/usr/bin/mico_aivs_lab` | 2910 | Stock Xiaomi cloud voice assistant (`108m` VSZ) | **Stop** |
| `/bin/ledserver` | 1190 | Stock LED daemon writing to `/sys/devices/i2c-0/0-003a/led_*` | **Stop** so `hm-gateway` exclusively owns the 18-LED ring |
| `/bin/touchpad` | 2109 | Stock button daemon reading `/dev/input/event0` (`adc_keypad`) | **Stop** (or `EVIOCGRAB` in `hm-gateway`) so key presses don't trigger stock actions |
| `/usr/bin/mediaplayer` / `/usr/bin/quickplayer` / `/usr/bin/sound_effect` / `/usr/bin/linein` | 1784 / 1181 / 1172 / 1300 | Stock playback daemons that rewrite `mysoftvol` (`numid=177`) and open `pcmC0D2p` | **Stop** |
| `/data/open-xiaoai/client ws://192.168.1.10:4412/...` + `sh /data/init.sh` | 2491 / 2142 | Legacy retired RUI client & 30s watchdog | **Stop & replace** in `/data/init.sh` when deploying |

---

## 3. Live Audio Pipeline & AEC Verification

### 3.1 ALSA Devices (`/proc/asound/pcm` & `/dev/snd/`)
- **Card 0**: `AMLAXGSOUND` (`AML-AXGSOUND`)
  - `pcmC0D0p` / `pcmC0D0c`: `TDM-A-dummy`
  - `pcmC0D1p` / `pcmC0D1c`: `TDM-B-dummy`
  - **`pcmC0D2p` / `pcmC0D2c` (`hw:0,2`)**: `TDM-C-tas5805 multicodec-2` (Playback to TAS5805 amp, `S16_LE`, 2ch, 48,000 Hz; `dmixer` uses `period_size 1920`, `buffer_size 7680`)
  - **`pcmC0D3c` (`hw:0,3`)**: `PDM-dummy dummy-3` (Capture, `S32_LE`, 8ch, 48,000 Hz; `Capture` uses `period_size 384`, `buffer_size 6144`)

### 3.2 Mixer Routing (`amixer -c 0 contents`)
- `numid=7` (`Loopback Enable`): `values=1` (`Enable`)
- `numid=8` (`Loopback datain source`): `values=4` (`PDMIN` — 6 PDM mics on `ch0..5`)
- `numid=9` (`Loopback tmdin lb source`): `values=1` (`TDMOUT_B` / internal loopback on `ch6..7`)
- `numid=6` (`pdm dclk`): `values=0` (`pdm dclk 3.072m, support 8k/16k/32k/48k`)
- `numid=4` (`PDM Filter Mode`): `values=1` (`Filter Mode 1`)
- `numid=177` (`mysoftvol`): `values=190,190` (`min=0, max=255`, `-51.0 dB .. 0.0 dB`, `0.20 dB/step`)

### 3.3 Capture Scaling & AEC Pipeline
- **Scaling**: `ch0..5` (6 PDM mics) use `sample >> 12`; `ch6..7` (2-ch digital playback reference, full scale `2^28`) use `sample >> 16`.
- **Sample Rate & AEC**:
  - Either open `hw:0,3` at **16,000 Hz** directly (supported by `numid=6`: `pdm dclk 3.072m, support 8k/16k/32k/48k` — we can verify with an on-device `pcmprobe` whether `hw:0,3` natively clocks at 16 kHz with clean `ch6/7` loopback!) **OR** capture at 48,000 Hz and 3:1 FIR-decimate to 16,000 Hz.
  - Feed 6 mic channels + 1 mono-summed reference (`(ch6 + ch7) / 2`) into `hm-gateway`'s existing SpeexDSP MDF pipeline (`speexdsp/`, 6 cancellers instead of 7).

---

## 4. Minimal Viable Adapter Architecture in `hm-gateway`

1. **64-bit `aarch64-unknown-linux-musl` Unified Target**:
   - Because LX06 runs a 64-bit `aarch64` Linux 4.9.61 kernel, compiling `hm-gateway` for **`aarch64-unknown-linux-musl`** (instead of 32-bit `armv7`) means:
     - Standard 64-bit `input_event` (24 bytes) and standard 64-bit ALSA ioctls work directly against the 64-bit kernel without any 32-bit compat layer quirks!
     - Dot2 continues to build for `armv7-unknown-linux-musleabihf` via `./tools/build_firmware.sh` completely unchanged.
2. **Mainline ALSA Struct Layout (`crates/gateway/src/firmware/alsa.rs`)**:
   - Keep Dot2's 604/608-byte vendor `snd_pcm_hw_params` under Dot2, and support mainline Linux `snd_pcm_hw_params` (608 bytes on `aarch64` with standard mainline offsets and `SNDRV_PCM_FORMAT_S32_LE = 10` / `S16_LE = 2`) for LX06.
3. **LX06 Hardware Port (`crates/gateway/src/firmware/`)**:
   - **Capture**: `/dev/snd/pcmC0D3c` (`hw:0,3`), `S32_LE`, 8ch (`ch0..5` mics `>>12`, `ch6..7` ref `>>16`).
   - **Playback**: `/dev/snd/pcmC0D2p` (`hw:0,2`), `S16_LE`, 2ch, 48 kHz (direct hardware device bypasses `dtsladspa` LADSPA plugin; set `mysoftvol` `numid=177` to fixed unity `255` at startup and use `hm-gateway`'s software volume + EQ).
   - **LEDs**: `Aw20054Ring` writing `"<idx> 0xBBGGRR\n"` (`0..17`) to `/sys/devices/i2c-0/0-003a/led_rgb`.
   - **Buttons**: `/dev/input/event0` (`adc_keypad`, non-standard stock key codes verified against `/etc/init.d/input` & `/usr/sbin/input.sh`) mapping:
     - `139` (`KEY_MENU`) → `vol_up`
     - `115` (`KEY_VOLUMEUP`) → `vol_down`
     - `102` (`KEY_HOME`) → `mute` (short press toggle mute, 1.5s hold → local Wi-Fi provisioning `#23`)
     - `114` (`KEY_VOLUMEDOWN`) → `action` (Play/Pause: stop/interrupt, 1.5s hold toggles `xiaoai-pinned` / `homemate`)
4. **DeviceProfile Preset (`crates/gateway/src/firmware/profile.rs`)**:
   - Add `DeviceProfile::lx06()` (`model: "LX06"`, `gateway_tag: "lx06-direct"`, `led_count: 18`, `key_map: KeyMap { mute: 102, vol_down: 115, vol_up: 139, action: 114 }`).

---

## 5. `hm-core` Deployment & Storage Topology on LX06

Because the kernel is **`aarch64` (4.9.61)** and available RAM is **~200 MB** (once stock Xiaomi daemons are stopped), **`hm-core`'s existing `aarch64-unknown-linux-musl` binary is natively compatible with LX06!**

The only physical constraint is **flash storage** (`/data` has 8.2 MB free out of 13.3 MB; `hm-core` is ~24.7 MB):

### Phase 1 (Zero-Reflash Bring-up & Acoustic Verification)
- Put `hm-gateway` (`aarch64-unknown-linux-musl`, ~3.4 MB) in `/data/hm-gateway/` (or `/tmp/hm-gateway/` during testing).
- Run `hm-core` on Mac/NAS and forward loopback `127.0.0.1:8932` via SSH reverse tunnel (`ssh -R 8932:127.0.0.1:8932 root@192.168.1.50`), OR temporarily stage `hm-core` in `/tmp` (`119.9 MB` free tmpfs!) to test **100% on-device `hm-core` + `hm-gateway` loopback execution** in RAM before touching SquashFS!
  - *Note*: `/tmp` has **119.9 MB free** and `available` RAM is **~200 MB** after stopping `mipns-xiaomi`/`mico_aivs_lab`! That means we can even test `hm-core` (24.7 MB in `/tmp`) + `hm-gateway` (3.4 MB in `/data`) **on-device over real loopback `127.0.0.1:8932`** without reflashing `system0`!

### Phase 2 (Permanent On-Device Production Flash)
- Repack `system0` SquashFS (`mtd4`, 40.125 MiB limit): remove unused stock Xiaomi cloud/voice binaries (`/usr/lib/libxaudio_engine.so` 4.28 MB, `/usr/bin/mipns-xiaomi`, `/usr/bin/mico_aivs_lab`, `/usr/share/xiaomi/*_model*`), embed `hm-core` + `hm-gateway` + `silero_vad.onnx` in `/usr/bin` (SquashFS XZ-compressed reduces 28 MB of ELF binaries to ~10–12 MB!), and store mutable state/SQLite DB in `/data/hm/` (8.2 MB free UBIFS).
- **Rollback**: `system1` (`mtd5`) remains 100% untouched stock firmware (`fw_env -s boot_part boot1 && reboot`).

---

## 6. Live Step 1–3 Verification Record (`192.168.1.50`, 2026-11-19)

- **Build artifact**: `tools/build_firmware_lx06.sh` (`aarch64-unknown-linux-musl`, `rust-lld` final link, `3,344,760` bytes = 19.9% of 16 MiB gate).
- **Compressed `/data` bundle**: `/data/hm/hm-bundle.tar.xz` (`7.2 MiB`, containing `hm-core` 18.5 MB with `tract-onnx`, `hm-gateway` 3.2 MB, `silero_vad.onnx` 629 KB, and `web/` console) + `/data/hm/unxz` (`29.2 KB` static `aarch64` helper) + `/data/init.sh` (`deploy/lx06/init.sh`).
- **Storage & Memory readback while running**:
  - `/data`: `9.5M / 13.3M` used, **`3.2M` available** (`75%`).
  - `/tmp`: `24.5M / 120.9M` used, **`96.4M` available** (`20%`).
  - RAM (`free -m`): `241 MB` total, `62 MB` used, **`146 MB` available**.
- **ALSA & Audio pipeline verification (100% silent, `--volume-cap 0.0`, `mysoftvol=0,0`)**:
  - Capture `hw:0,3` (`pcmC0D3c`): `S32_LE`, 8ch, 48,000 Hz, `period=512`, `buffer=4096` (`131,072` B hardware max).
  - Playback `hw:0,2` (`pcmC0D2p`): `S16_LE`, 2ch, 48,000 Hz, `period=2048`, `buffer=8192`, `state: RUNNING`.
  - `[boot-health] blocks=75 mic_nonzero=7/7 ch8_nonzero=false first_play_corr=n/a verdict=no_playback_yet`.
  - `[aec] dealias locked=false ever=false relocks=0 losses=0 backoffs=0 degraded_blocks=0`.
- **Core Loopback & Management API (`http://192.168.1.50:8930/api/v0/status`)**:
  - `connected: true`, `protocol_operating: 2`, `stable_id: "lx06"`, `model: "LX06"`, `gateway: "home-mate-device-gateway 0.0.53 (lx06-direct)"`, `wifi_ssid: "Home-WiFi"`, `echo_health: "healthy"`, `tail_mismatch: false`, `volume: 0.0`.
- **Host test suite**: 194 lib unit tests + 8 protocol contract tests + 62 firmware contract tests = **264 passed, 0 failed**.

