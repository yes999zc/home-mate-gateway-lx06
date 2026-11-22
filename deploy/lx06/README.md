# LX06 idle wake selection

In the default mode, “小爱同学” wakes the stock assistant and the wake word configured in Home Mate starts a Home Mate turn. Say the command immediately after the wake word. Each assistant keeps its own conversation, follow-up and playback. A long hold of the Play/Pause key (1.5 seconds) pins the native assistant. Hold it again to return to dual wake. An old saved `xiaoai` selection migrates to dual wake on first boot.

The stock microphone alias `noop` and speaker alias `plug:dmixer` on the tested LX06 firmware allow the stock assistant and Gateway to share the hardware. Gateway forwards idle keyword candidates through the existing Core protocol. Once Core accepts a Home wake, Gateway pauses the native wake service for that turn. A native dialog, ongoing playback, an unknown native state or a microphone mute prevents a new Home claim. The native touchpad continues to handle the physical mute and volume keys. An interrupted Home playback can leave audio already submitted to the shared mixer. Gateway reports the resulting flush as unconfirmed.

## Packaging

Build the gateway with `./tools/build_firmware_lx06.sh`. Pair the resulting AArch64 binary as `hm-gateway` inside the existing `hm-bundle.tar.xz` with the supervisor from this directory for a cold start. For serial 87, use the verified release Core, web assets, Silero model, and KWS driver, models and libraries. Package `deploy/lx06/kws-run.sh` as `run.sh` in the LX06 KWS archive. Store the KWS URL in `/data/hm/kws.url`, its lowercase SHA-256 in `/data/hm/kws.sha256`, and the main archive SHA-256 in `/data/hm/hm-bundle.sha256`. Preserve the board's unxz decoder. The launcher loads Silero from the main RAM bundle with gain 3 and 1 ms onset and silence durations. After boot, verify `devices[].mic.vad.endpoint.kind` is `gateway_vad` through `/api/v0/mode`. The supervisor checks the unpacked gateway's shared-audio contract before activation.

The main archive in `/data/hm` expands into `/tmp/hm`. The separate KWS archive expands into `/tmp/hm-kws`. Both survive assistant selection and manual mode changes until reboot. A failed KWS download is retried from the configured URL. The native assistant stays available during startup subject to microphone mute. Run `python3 tools/test_lx06_supervisor.py` for the host-side cold boot, upgrade, retry and cache checks.

## Device acceptance

On an owner-authorized LX06, record whether each wake word followed by a command in one breath reaches its intended assistant, including the first request after cold boot and after a manual mode change. Try the other wake word during each assistant's listening, follow-up and playback. Exercise physical and managed mute, volume and the long-press fallback, then check native wake resumes after a Home playback tail. Record cold-start and consecutive-turn latency, free RAM, CPU and shared audio underruns on the target unit before release.
