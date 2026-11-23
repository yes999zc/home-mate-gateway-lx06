#!/bin/sh
# Home Mate on-device supervisor for Xiaomi AI Speaker Pro (LX06).
#
# Keep the main bundle compressed in flash and unpack it into tmpfs on boot.
# The independently fetched keyword spotter also lives in tmpfs. Both caches
# survive a change between normal dual-wake operation and pinned native mode.
# Mutable Core and gateway state lives in /data/hm/state.

export PATH=/usr/sbin:/usr/bin:/sbin:/bin

HM_FLASH_DIR=/data/hm
HM_STATE_DIR=/data/hm/state
HM_RUN_DIR=/tmp/hm
KWS_DIR=/tmp/hm-kws
NATIVE_MUTE=/tmp/mipns/mute
NATIVE_LOG_DIR=/tmp/mico_aivs_lab
CORE_LOG=/tmp/hm-core.log
GW_LOG=/tmp/hm-gateway.log
WD_LOG=/tmp/hm-watchdog.log
INIT_D_DIR=/etc/init.d
SHUT_LED_BIN=/bin/shut_led

# Only the dual-wake gateway coordinates pns while it is running. A pinned
# native session has all of its original services, including the wake engine.
# Stock /etc/init.d/led's service_started() always triggers `/bin/show_led 4`
# (the boot blue ring) on every `start` call. Skip restarting ledserver when
# it is already live and dismiss L=4 (boot blue ring only, preserving L=7 mute).
start_native_services() {
    for svc in led touchpad sound_effect mediaplayer quickplayer mico_aivs_lab mibrain_service messagingagent "$@"; do
        if [ "$svc" = led ]; then
            if ! pidof ledserver >/dev/null 2>&1 && [ -x "$INIT_D_DIR/$svc" ]; then
                "$INIT_D_DIR/$svc" start >> "$WD_LOG" 2>&1 || true
            fi
            if [ -x "$SHUT_LED_BIN" ]; then
                "$SHUT_LED_BIN" 4 >> "$WD_LOG" 2>&1 || true
            fi
            continue
        fi
        if [ -x "$INIT_D_DIR/$svc" ]; then
            "$INIT_D_DIR/$svc" start >> "$WD_LOG" 2>&1 || true
        fi
    done
}

# The native daemon creates its instruction journal on the first instruction.
# Prepare its empty starting state after the daemon creates its directory.
# Retry on later ticks if necessary while the gateway continues to serve buttons.
prepare_native_journal() {
    [ "${journal_initialized:-0}" = 0 ] || return 0
    [ -d "$NATIVE_LOG_DIR" ] || return 1
    journal="$NATIVE_LOG_DIR/instruction.log"
    if [ ! -e "$journal" ]; then
        (umask 022; set -C; : > "$journal") || [ -f "$journal" ] || return 1
    fi
    [ -f "$journal" ] || return 1
    journal_initialized=1
}

pns_service() {
    /etc/init.d/pns "$1" >> "$WD_LOG" 2>&1
}

restore_native_wake() {
    # The existing gateway journal has exactly two fields, in either order.
    # Keep pns stopped on an unreadable/corrupt latch even if hm-core or the
    # flash bundle is unavailable. Native hardware mute has its own latch.
    # This BusyBox tr treats POSIX classes as literal bytes. Normalize JSON
    # whitespace to spaces and allow it only between tokens.
    if tr '\011\012\015' '   ' < "$HM_STATE_DIR/state.json" |
        grep -Eq '^ *\{ *("muted" *: *false *, *"volume" *: *[0-9]+|"volume" *: *[0-9]+ *, *"muted" *: *false) *\} *$'; then
        if [ ! -e "$NATIVE_MUTE" ] && ! pidof mipns-xiaomi >/dev/null 2>&1; then
            pns_service start || return 1
        fi
    elif pidof mipns-xiaomi >/dev/null 2>&1; then
        pns_service stop || return 1
    fi
}

runtime_pid() {
    binary=$1
    subcommand=$2
    for pid in $(pidof "$binary" 2>/dev/null); do
        # Match argv rather than ps display width. The button watcher and
        # the firmware have the same executable.
        if [ "$(tr '\000' '\n' < "/proc/$pid/cmdline" 2>/dev/null | sed -n '2p')" = "$subcommand" ]; then
            echo "$pid"
            return 0
        fi
    done
    return 1
}

stop_runtime() {
    old_pid=$(runtime_pid hm-gateway "$1") || return 0
    kill "$old_pid" 2>/dev/null || true
    for attempt in 1 2 3; do
        kill -0 "$old_pid" 2>/dev/null || return 0
        sleep 1
    done
    if [ "$(runtime_pid hm-gateway "$1")" = "$old_pid" ]; then
        kill -KILL "$old_pid" 2>/dev/null || true
    fi
}

unpack_bundle_if_needed() {
    main_digest=""
    if [ -f "$HM_FLASH_DIR/hm-bundle.sha256" ]; then
        main_digest=$(cat "$HM_FLASH_DIR/hm-bundle.sha256")
        case "$main_digest" in *[!0-9a-f]*) return 1 ;; esac
        [ "${#main_digest}" -eq 64 ] || return 1
    fi
    if [ -f "$HM_RUN_DIR/.lx06-idle-wake-v1" ] && [ -x "$HM_RUN_DIR/hm-core" ] &&
        [ -x "$HM_RUN_DIR/hm-gateway" ] &&
        [ "$(cat "$HM_RUN_DIR/.main-sha256" 2>/dev/null)" = "$main_digest" ]; then
        return 0
    fi
    if [ -n "$main_digest" ]; then
        [ "$(sha256sum "$HM_FLASH_DIR/hm-bundle.tar.xz" | cut -d ' ' -f 1)" = "$main_digest" ] || return 1
    fi
    mkdir -p "$HM_STATE_DIR/models"
    echo "$(date) unpacking $HM_FLASH_DIR/hm-bundle.tar.xz -> $HM_RUN_DIR" >> "$WD_LOG"
    stage="$HM_RUN_DIR.part"
    rm -rf "$stage"
    mkdir -p "$stage" || return 1
    # Check both stages on BusyBox sh. Stream the xz directly from flash;
    # there is no room for another copy of the compressed archive in /data.
    ( "$HM_FLASH_DIR/unxz" < "$HM_FLASH_DIR/hm-bundle.tar.xz" && touch "$stage/.unpacked" ) |
        tar -xf - -C "$stage" &&
        [ -f "$stage/.unpacked" ] && [ -s "$stage/hm-core" ] && [ -s "$stage/hm-gateway" ] &&
        chmod +x "$stage/hm-core" "$stage/hm-gateway" || return 1
    # Old LX06 binaries take exclusive ownership of audio. Only activate a
    # freshly unpacked archive after its gateway confirms the shared contract.
    [ "$("$stage/hm-gateway" lx06-wake-contract 2>> "$WD_LOG")" = lx06-idle-wake-v1 ] &&
        touch "$stage/.lx06-idle-wake-v1" || return 1
    echo "$main_digest" > "$stage/.main-sha256" || return 1
    # The live directory is a disposable tmpfs cache, never persistent state.
    rm -rf "$HM_RUN_DIR"
    mv "$stage" "$HM_RUN_DIR" || return 1
    ln -sf "$HM_RUN_DIR/silero_vad.onnx" "$HM_STATE_DIR/models/silero_vad.onnx"
}

sync_kws_config() {
    mkdir -p "$HM_STATE_DIR/kws-persist"
    for f in kw.txt kw.exit.json kw.tuning.json kw.bundle.json; do
        disk="$HM_STATE_DIR/kws-persist/$f"
        live="$KWS_DIR/$f"
        if [ -s "$live" ]; then
            cmp -s "$live" "$disk" || cp "$live" "$disk"
        elif [ -s "$disk" ]; then
            cp "$disk" "$live"
        fi
    done
}

fetch_kws_if_needed() {
    [ -s "$HM_FLASH_DIR/kws.sha256" ] || return 1
    digest=$(cat "$HM_FLASH_DIR/kws.sha256")
    case "$digest" in *[!0-9a-f]*) return 1 ;; esac
    [ "${#digest}" -eq 64 ] || return 1
    if [ -x "$KWS_DIR/run.sh" ] && [ -x "$KWS_DIR/hm-kws" ] &&
        [ "$(cat "$KWS_DIR/.archive-sha256" 2>/dev/null)" = "$digest" ]; then
        sync_kws_config
        return 0
    fi
    [ -s "$HM_FLASH_DIR/kws.url" ] || return 1
    url=$(tr -d '\r\n' < "$HM_FLASH_DIR/kws.url")
    [ -n "$url" ] || return 1
    stage="$KWS_DIR.part"
    rm -rf "$stage"
    mkdir -p "$stage" || return 1
    archive="$KWS_DIR.download"
    if curl -fsSL --connect-timeout 5 --max-time 60 -o "$archive" "$url" &&
        [ "$(sha256sum "$archive" | cut -d ' ' -f 1)" = "$digest" ] &&
        ( "$HM_FLASH_DIR/unxz" < "$archive" && touch "$stage/.unpacked" ) |
        tar -xf - -C "$stage" &&
        [ -f "$stage/.unpacked" ] &&
        [ -s "$stage/run.sh" ] && [ -s "$stage/hm-kws" ] &&
        [ -s "$stage/ld-linux-aarch64.so.1" ]; then
        rm -f "$archive"
        echo "$digest" > "$stage/.archive-sha256" || return 1
        chmod +x "$stage/run.sh" "$stage/hm-kws" "$stage/ld-linux-aarch64.so.1" || return 1
        for f in kw.txt kw.exit.json kw.tuning.json kw.bundle.json; do
            if [ -s "$HM_STATE_DIR/kws-persist/$f" ]; then
                cp "$HM_STATE_DIR/kws-persist/$f" "$stage/$f" || return 1
            fi
        done
        rm -rf "$KWS_DIR"
        mv "$stage" "$KWS_DIR" || return 1
        echo "$(date) keyword spotter ready" >> "$WD_LOG"
    else
        rm -f "$archive"
        rm -rf "$stage"
        echo "$(date) keyword spotter fetch or unpack failed" >> "$WD_LOG"
        return 1
    fi
}

init_state_if_needed() {
    mkdir -p "$HM_STATE_DIR" /tmp/hm-voice-events
    if [ ! -L "$HM_STATE_DIR/voice-events" ]; then
        rm -rf "$HM_STATE_DIR/voice-events"
        ln -s /tmp/hm-voice-events "$HM_STATE_DIR/voice-events"
    fi
    if [ ! -s "$HM_STATE_DIR/device-id" ]; then
        printf 'lx06\n' > "$HM_STATE_DIR/device-id"
    fi
    if [ ! -e "$HM_STATE_DIR/state.json" ]; then
        printf '{"volume":100,"muted":false}\n' > "$HM_STATE_DIR/state.json"
    fi
    # Older supervisors saved whichever assistant the user had last selected.
    # Both old modes now start with both wake words available. Only a new
    # explicit hardware pin survives a reboot as native-only.
    if [ ! -s "$HM_FLASH_DIR/mode" ] || [ "$(cat "$HM_FLASH_DIR/mode")" = xiaoai ]; then
        printf 'homemate\n' > "$HM_FLASH_DIR/mode.new" &&
            mv "$HM_FLASH_DIR/mode.new" "$HM_FLASH_DIR/mode"
    fi
}

# One iteration is shared by boot, hot selection and supervisor recovery.
supervise_tick() {
    mode=$(tr -d '\r\n ' < "$HM_FLASH_DIR/mode" 2>/dev/null)
    case "$mode" in
        xiaoai-pinned)
            if [ "$last_mode" != xiaoai ]; then
                stop_runtime firmware
                start_native_services
                last_mode=xiaoai
            fi
            restore_native_wake || return 1
            unpack_bundle_if_needed || return 1
            if ! runtime_pid hm-gateway lx06-switch-watch >/dev/null; then
                "$HM_RUN_DIR/hm-gateway" lx06-switch-watch /dev/input/event0 "$HM_FLASH_DIR/mode" >> "$WD_LOG" 2>&1 &
            fi
            return 0
            ;;
        homemate) ;;
        *) return 1 ;;
    esac

    if [ "$last_mode" != homemate ]; then
        stop_runtime lx06-switch-watch
        start_native_services
        last_mode=homemate
    fi

    prepare_native_journal || true
    if ! runtime_pid hm-gateway firmware >/dev/null; then
        restore_native_wake || return 1
    fi
    unpack_bundle_if_needed || return 1
    if ! pidof hm-core >/dev/null 2>&1; then
        HM_DATA_DIR="$HM_STATE_DIR" HM_BIND="0.0.0.0:8930" HM_GATEWAY_BIND="127.0.0.1:8932" \
            HM_WEB_DIR="$HM_RUN_DIR/web" HM_SILERO_VAD="$HM_RUN_DIR/silero_vad.onnx" \
            HM_LOOP_DIR=/tmp/hm-loop "$HM_RUN_DIR/hm-core" >> "$CORE_LOG" 2>&1 &
    fi

    if ! runtime_pid hm-gateway firmware >/dev/null; then
        if ! fetch_kws_if_needed; then
            return 1
        fi
        vol_cap=1.0
        [ ! -f "$HM_FLASH_DIR/silent" ] || vol_cap=0.0
        HM_FW_AUDIO_CPU=1 HM_FW_PLAY_CPU=0 HM_LOOP_DIR=/tmp/hm-loop \
            KWS_VAD_MODEL="$HM_RUN_DIR/silero_vad.onnx" HM_LX06_DUAL_WAKE=1 \
            "$HM_RUN_DIR/hm-gateway" firmware --core 127.0.0.1:8932 --device-id lx06 \
            --state-dir "$HM_STATE_DIR" --aec --barge-in off --loudness-bass 0.0 \
            --loopback-probe --kws --kws-dir "$KWS_DIR" --kws-cpu 3 \
            --volume-cap "$vol_cap" >> "$GW_LOG" 2>&1 &
    else
        sync_kws_config
    fi
}

run_supervisor() {
    sleep 5
    init_state_if_needed
    last_mode=""
    while true; do
        supervise_tick
        # Keep runtime logs from crowding the RAM-backed KWS and main bundles.
        use=$(df -k /tmp | sed -n 2p | tr -s " " | cut -d " " -f5 | tr -d "%")
        if [ "${use:-0}" -ge 85 ]; then
            : > "$CORE_LOG"
            : > "$GW_LOG"
            : > "$WD_LOG"
            rm -f /tmp/hm-voice-events/voice-events-*.jsonl 2>/dev/null || true
        fi
        sleep 1
    done
}

run_supervisor &
