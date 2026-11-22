use super::keywords::deploy_keywords;
use super::*;

#[test]
fn firmware_core_address_must_be_loopback() {
    assert!(core_addr_is_loopback("127.0.0.1:8932"));
    assert!(core_addr_is_loopback("localhost:8932"));
    assert!(core_addr_is_loopback("[::1]:8932"));
    assert!(!core_addr_is_loopback("192.168.1.20:8932"));
    assert!(!core_addr_is_loopback("0.0.0.0:8932"));
    assert!(!core_addr_is_loopback("not-an-address"));
}

#[test]
fn wifi_conf_rejects_quotes_backslashes_and_controls() {
    assert!(wifi_credentials_rejected("home", "correct-horse").is_none());
    assert!(wifi_credentials_rejected("", "correct-horse").is_some());
    assert!(wifi_credentials_rejected("home", "short").is_some());
    assert!(wifi_credentials_rejected("ho\"me", "correct-horse").is_some());
    assert!(wifi_credentials_rejected("home", "correct\\horse").is_some());
    assert!(
        wifi_credentials_rejected("home\nnetwork={\n\tkey_mgmt=NONE\n#", "correct-horse").is_some()
    );
    assert!(wifi_credentials_rejected("home\r", "correct-horse").is_some());
}

/// M4: the write site's live set is what the watchdog reports — the
/// top-of-loop copy predates the command drain, which is exactly the
/// first-write blind spot (a play whose very first period parks used to
/// publish mask 0 and never be named).
#[test]
fn play_progress_records_the_live_set_at_the_write_site() {
    let p = PlayProgress::default();
    // Stale live mask (the play has not been drained yet) — the write
    // itself says which channels it blocks.
    p.set_live(0, false);
    p.begin_write(0b01);
    let t = kws::mono_ms();
    assert_eq!(
        p.stalled(t, 60_000),
        None,
        "a fresh write is not a stall yet"
    );
    assert_eq!(
        p.stalled(t + 60_000, 60_000),
        Some(0b01),
        "the write mask names the blocked channel"
    );
    p.end_write();
    assert_eq!(
        p.stalled(t + 60_000, 60_000),
        None,
        "no outstanding write (a starve) is not a stall"
    );
    // An idle silence-clock write that wedges owes no receipt, but it is
    // still a dead DAC path.
    p.begin_write(0);
    // The second write starts a new watchdog interval and may cross a tick.
    let t = kws::mono_ms();
    assert_eq!(p.stalled(t + 60_000, 60_000), Some(0));
    p.clear();
    assert_eq!(p.stalled(t + 60_000, 60_000), None);
    // 0 = watchdog off.
    p.begin_write(0b11);
    assert_eq!(p.stalled(t + 60_000, 0), None);
}

// S24_3LE decode keeps the full 24-bit value (sign-extended).
#[test]
fn decode_s24_3le_sign_and_scale() {
    let mut raw = vec![];
    // channel 0: +max (ff ff 7f), channel 1: −1 (ff ff ff)
    raw.extend_from_slice(&[0xff, 0xff, 0x7f, 0xff, 0xff, 0xff]);
    let out = decode_s24_3le(&raw, 2);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], 8_388_607);
    assert_eq!(out[1], -1);
}

#[test]
fn downmix_means_mic_channels_and_appends_ref() {
    // 2 frames × 3 channels; mics = [0], ref = 2.
    let mut raw = vec![];
    for (c0, c2) in [(100i32 << 4, 20000i32 << 4), (200i32 << 4, 21000i32 << 4)] {
        for v in [c0, 0, c2] {
            // 24-bit LE sample; ×16 prescale so >>4 in the downmix
            // recovers the nominal value.
            raw.extend_from_slice(&[
                (v & 0xff) as u8,
                ((v >> 8) & 0xff) as u8,
                ((v >> 16) & 0xff) as u8,
            ]);
        }
    }
    // The 24-bit value (v<<8 in the fixture) maps 1:1 onto i16 with
    // saturation — v=20000 stays 20000; v=30000 would clamp at 32767.
    let mono = downmix_block(&raw, 3, &[0], 2, false);
    assert_eq!(mono, vec![100, 200]);
    let two = downmix_block(&raw, 3, &[0], 2, true);
    assert_eq!(two, vec![100, 20000, 200, 21000]);
}

#[test]
fn mix_applies_gain_clamps_and_duplicates() {
    let a = vec![i16::MAX, 1000];
    let b = vec![10, -10];
    // unity gain: sum clamps at the ceiling
    let s = mix_to_stereo(&a, &b, 2, 1.0);
    assert_eq!(s, vec![i16::MAX, i16::MAX, 990, 990]);
    // gain 2.0 keeps the clamp honest
    let hot = mix_to_stereo(&a, &b, 2, 2.0);
    assert_eq!(hot[0], i16::MAX);
    // level 115 = −6 dB ≈ ×0.5012
    let quiet = mix_to_stereo(&[10000], &[], 1, level_to_gain(115));
    assert!((quiet[0] as i32 - 5012).abs() < 6, "got {}", quiet[0]);
}

#[test]
fn volume_law_persists_and_scales() {
    assert!((level_to_gain(127) - 1.0).abs() < 1e-6);
    assert!((level_to_gain(0) - 10f32.powf(-63.5 / 20.0)).abs() < 1e-7);
    let dir = std::env::temp_dir().join(format!("hm-fw2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    save_state(&dir, 42, true);
    assert_eq!(load_state(&dir), (42, true));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(load_state(&dir), (DEFAULT_VOLUME, false));
}

// --- R1d local stop --------------------------------------------------

#[test]
fn a_local_stop_needs_a_live_trusted_play_past_protection() {
    // The happy path: full duplex, live voice play, 900 ms in, trusted fact.
    assert!(local_stop_allowed(true, true, false, true, 900, 800, false));
    // R1g: on a HALF-duplex link the reflex never cuts the audio, however
    // good the fact is — the wake word is the only command entry.
    assert!(
        !local_stop_allowed(true, false, false, true, 900, 800, false),
        "half duplex"
    );
    // Every gate is necessary — drop one and the audio must survive.
    assert!(
        !local_stop_allowed(false, true, false, true, 900, 800, false),
        "no voice play"
    );
    assert!(
        !local_stop_allowed(true, true, true, true, 900, 800, false),
        "privacy mute"
    );
    assert!(
        !local_stop_allowed(true, true, false, false, 900, 800, false),
        "untrusted fact"
    );
    assert!(
        !local_stop_allowed(true, true, false, true, 0, 800, false),
        "TTS onset transient"
    );
    assert!(
        !local_stop_allowed(true, true, false, true, 799, 800, false),
        "one ms before protection ends"
    );
    assert!(
        !local_stop_allowed(true, true, false, true, 900, 800, true),
        "second stop in one episode"
    );
    // Exactly at the boundary the play is old enough (>= not >).
    assert!(local_stop_allowed(true, true, false, true, 800, 800, false));
}

#[test]
fn a_local_flush_is_its_own_fact_and_keeps_a_newer_plays_slot() {
    // The reflex cut "answer": its flush must report StoppedLocally.
    let out = flush_outcome(None, Some(("answer", 1_000)), Some("answer"));
    assert_eq!(
        out,
        FlushOutcome::LocallyStopped {
            task_id: "answer".into(),
            fired_ms: 1_000,
            takes_slot: true
        }
    );
    assert!(out.takes_slot());

    // A new play took the slot before the flush landed: the fact still
    // names the play that was cut, but the new owner keeps its slot.
    let out = flush_outcome(None, Some(("answer", 1_000)), Some("next"));
    assert_eq!(
        out,
        FlushOutcome::LocallyStopped {
            task_id: "answer".into(),
            fired_ms: 1_000,
            takes_slot: false
        }
    );
    assert!(
        !out.takes_slot(),
        "the next play's StreamDone owns that slot"
    );
}

#[test]
fn a_core_named_stop_still_owns_its_receipt() {
    // Unchanged behaviour: the stop the core asked for is confirmed only
    // while it still owns the slot.
    assert_eq!(
        flush_outcome(Some("cue"), None, Some("cue")),
        FlushOutcome::NamedStop {
            task_id: "cue".into(),
            takes_slot: true
        }
    );
    assert_eq!(
        flush_outcome(Some("cue"), None, Some("answer")),
        FlushOutcome::Unclaimed
    );
    assert_eq!(
        flush_outcome(None, None, Some("answer")),
        FlushOutcome::Unclaimed
    );
    // A local stop outranks a pending stop for the same channel: the
    // core's Stop for the same task arrives later and finds nothing.
    assert!(matches!(
        flush_outcome(Some("answer"), Some(("answer", 5)), Some("answer")),
        FlushOutcome::LocallyStopped { .. }
    ));
}

/// R1j/P1: the deploy validates against the MODEL's token table before
/// touching anything, backs up the previous file and installs
/// atomically. A rejected deploy leaves the old vocabulary intact.
#[test]
fn keyword_deploy_validates_tokens_and_installs_atomically() {
    let dir = std::env::temp_dir().join(format!(
        "hm-kws-deploy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("model")).unwrap();
    std::fs::write(
        dir.join("model/tokens.txt"),
        "n 10\nǐ 11\nh 12\nǎo 13\nā 14\n",
    )
    .unwrap();
    std::fs::write(dir.join("kw.txt"), "old @旧\n").unwrap();

    // Unknown token: named rejection, nothing written.
    let (ok, hash, n, detail, local_exit) = deploy_keywords(&dir, "n ǐ z z z z z @假词\n", &[], "");
    assert!(!ok);
    assert_eq!(hash, "");
    assert_eq!(n, 0);
    assert_eq!(local_exit, 0);
    assert!(detail.contains("unknown token 'z'"), "{detail}");
    assert_eq!(
        std::fs::read_to_string(dir.join("kw.txt")).unwrap(),
        "old @旧\n"
    );
    assert!(
        !dir.join("kw.txt.bak").exists(),
        "a rejected deploy must not touch the backup"
    );

    // Missing text / malformed line.
    let (ok, _, _, detail, _) = deploy_keywords(&dir, "n ǐ h ǎo  \n", &[], "");
    assert!(!ok);
    assert!(detail.contains("expected"), "{detail}");

    // Valid: accepted, previous file backed up, new file installed.
    let (ok, hash, n, _, _) = deploy_keywords(&dir, "n ǐ h ǎo @你好\n", &[], "");
    assert!(ok);
    assert_eq!(n, 1);
    assert_eq!(hash, fnv1a_hex("n ǐ h ǎo @你好\n".as_bytes()));
    assert_eq!(
        std::fs::read_to_string(dir.join("kw.txt")).unwrap(),
        "n ǐ h ǎo @你好\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("kw.txt.bak")).unwrap(),
        "old @旧\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Contract v2 §2.5 (#5): the deploy also installs WHICH words mean
/// "exit" — only those actually in the vocabulary — with the map's
/// version. A word the spotter cannot hear is named, never armed, and a
/// later deploy without retreat words removes the set (a stale word must
/// not keep cutting the audio).
#[test]
fn the_deploy_arms_only_the_retreat_words_it_installed() {
    let dir = std::env::temp_dir().join(format!(
        "hm-kws-exit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("model")).unwrap();
    std::fs::write(
        dir.join("model/tokens.txt"),
        "n 10\nǐ 11\nh 12\nǎo 13\nt 14\níng 15\n",
    )
    .unwrap();
    let words = "n ǐ h ǎo @你好\nn ǐ h ǎo @您好\nt íng @停止播放\n";

    // Two retreat words, only one of them in the file.
    let retreat = vec!["停止播放".to_string(), "不存在的词".to_string()];
    let (ok, _, n, detail, armed) = deploy_keywords(&dir, words, &retreat, "map-v1");
    assert!(ok, "{detail}");
    assert_eq!(n, 3);
    assert_eq!(
        armed, 1,
        "only a word the spotter can hear is armed: {detail}"
    );
    assert!(
        detail.contains("不存在的词"),
        "the dropped word must be named: {detail}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("kw.exit.json")).unwrap()).unwrap();
    assert_eq!(doc["hash"], serde_json::json!("map-v1"));
    assert_eq!(doc["words"], serde_json::json!(["停止播放"]));

    // A rejected deploy leaves the armed set exactly as it was.
    let (ok, _, _, _, _) = deploy_keywords(&dir, "n ǐ z @坏词\n", &["坏词".to_string()], "map-v2");
    assert!(!ok);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("kw.exit.json")).unwrap()).unwrap();
    assert_eq!(
        doc["hash"],
        serde_json::json!("map-v1"),
        "a rejected deploy changes nothing"
    );

    // No retreat words (or an older core): the set is removed, so nothing
    // can be cut locally on a stale word.
    let (ok, _, _, _, armed) = deploy_keywords(&dir, "n ǐ h ǎo @你好\n", &[], "");
    assert!(ok);
    assert_eq!(armed, 0);
    assert!(
        !dir.join("kw.exit.json").exists(),
        "the stale set must be gone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two repos must hash the same bytes to the same string — that is
/// what makes "the device has the file the core compiled" checkable.
#[test]
fn the_deploy_hash_matches_the_cores_fnv1a() {
    // hm-core::config::fnv1a_hex(b"abc") — pinned here because the repos
    // build independently.
    assert_eq!(fnv1a_hex(b"abc"), "e71fa2190541574b");
    assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
}

// --- #5 local exit ---------------------------------------------------

/// Contract v2 §2.5 (#5): which channels a retreat hit must flush, and
/// whether a flush still owns the slot it cut. The slot rule is the R1d
/// one: a newer play that took the channel keeps its slot and its facts.
#[test]
fn a_local_exit_flushes_every_live_channel_and_only_its_own_slot() {
    // Nothing playing (slot owners can lag a stopped play): nothing to do.
    assert_eq!(LocalExit::channel_mask([false, false], [None, None]), 0);
    // Voice only, music only, both.
    assert_eq!(LocalExit::channel_mask([true, false], [None, None]), 1);
    assert_eq!(LocalExit::channel_mask([false, true], [None, None]), 2);
    assert_eq!(LocalExit::channel_mask([true, true], [None, None]), 3);
    // A slot with no live write is still audio the person can hear.
    assert_eq!(
        LocalExit::channel_mask([false, false], [Some("A"), None]),
        1
    );

    let ex = LocalExit {
        keyword: "停止播放".into(),
        fired_ms: 1_000,
        flushed: 3,
        pending: 3,
        voice_task: Some("answer".into()),
        music_task: None,
        flush_err: None,
    };
    assert_eq!(
        ex.channels(),
        vec!["voice".to_string(), "music".to_string()]
    );
    assert!(ex.clears_slot(0, Some("answer")));
    assert!(
        !ex.clears_slot(0, Some("newer")),
        "a newer play keeps its slot"
    );
    assert!(!ex.clears_slot(0, None));
    assert!(
        !ex.clears_slot(1, Some("music-1")),
        "no task recorded for music"
    );
}

#[test]
fn block_dbfs_puts_the_phrase_above_the_room() {
    // Full-scale square wave = 0 dBFS; a tenth of that = -20 dB; silence
    // is the window's empty value, not a very quiet sound.
    assert!((block_dbfs(&[32767, -32768, 32767, -32768]) - 0.0).abs() < 0.1);
    assert!((block_dbfs(&[3277; 8]) - (-20.0)).abs() < 0.1);
    assert_eq!(block_dbfs(&[0; 16]), -120.0);
    assert_eq!(block_dbfs(&[]), -120.0);
}

#[test]
fn the_level_window_peaks_over_the_phrase_and_not_the_silence() {
    let mut w = LevelWindow::default();
    assert_eq!(w.peak(), None, "an unmeasured mic is not a quiet room");
    // The phrase arrives, then 400 ms of room tone: a claim read afterwards
    // must still describe the phrase (the driver reports up to lag_ms late).
    for _ in 0..5 {
        w.push(-28.0);
    }
    for _ in 0..5 {
        w.push(-55.0);
    }
    assert_eq!(w.peak(), Some(-28.0));
    // Loudest block wins regardless of order.
    w.push(-9.0);
    assert_eq!(w.peak(), Some(-9.0));
    // And the window really does forget: fill it with quiet blocks.
    for _ in 0..LEVEL_WINDOW_BLOCKS {
        w.push(-60.0);
    }
    assert_eq!(w.peak(), Some(-60.0), "old loud blocks must age out");
}

#[test]
fn a_dead_mic_window_is_absent_not_minus_infinity() {
    let mut w = LevelWindow::default();
    for _ in 0..LEVEL_WINDOW_BLOCKS {
        w.push(-120.0);
    }
    assert_eq!(w.peak(), None);
    assert_eq!(w.peak_over_ms(720), None);
}

#[test]
fn level_window_peak_over_ms_ignores_older_pre_phrase_transient_spikes() {
    let mut w = LevelWindow::default();
    // Pre-phrase transient spike (e.g. door slam / cough) followed by
    // 9 blocks (720 ms) of the actual wake phrase + trailing room tone.
    w.push(-4.0);
    for _ in 0..3 {
        w.push(-50.0);
    }
    // Wake phrase: 5 blocks at -32.0 dBFS, then 4 blocks at -48.0 dBFS (total 9 blocks = 720 ms).
    for _ in 0..5 {
        w.push(-32.0);
    }
    for _ in 0..4 {
        w.push(-48.0);
    }

    assert_eq!(
        w.peak(),
        Some(-4.0),
        "full 1.6s history still includes the older transient spike"
    );
    assert_eq!(
        w.peak_over_ms(720),
        Some(-32.0),
        "720ms wake-phrase span must ignore the pre-phrase -4.0 dBFS spike"
    );
    assert_eq!(
        w.peak_over_ms(0),
        Some(-48.0),
        "zero window clamps to at least 1 block (80ms)"
    );
    assert_eq!(
        w.peak_over_ms(10_000),
        Some(-4.0),
        "oversized window clamps to LEVEL_WINDOW_BLOCKS"
    );
}

#[test]
fn only_play_frames_carry_a_task_id_for_the_stale_guard() {
    assert_eq!(
        stream_task_of(&CoreFrame::PlayEnd {
            task_id: "a".into()
        }),
        Some("a")
    );
    assert_eq!(
        stream_task_of(&CoreFrame::PlayChunk {
            task_id: "a".into(),
            seq: 0,
            audio_b64: String::new()
        }),
        Some("a")
    );
    assert_eq!(
        stream_task_of(&CoreFrame::Stop {
            task_id: "a".into(),
            flush: true
        }),
        None,
        "a Stop is the core's own instruction, never a stale chunk"
    );
    assert_eq!(stream_task_of(&CoreFrame::TimeSet { ts_ms: 0 }), None);
}

#[test]
fn rss_budget_constants_and_check() {
    assert_eq!(RSS_WARN_KB, 16_384);
    assert_eq!(RSS_LIMIT_KB, 32_768);
    const { assert!(RSS_WARN_KB < RSS_LIMIT_KB) };
    check_rss_budget(8_192, "test ok");
    check_rss_budget(RSS_WARN_KB, "test warn");
    check_rss_budget(RSS_LIMIT_KB, "test limit");
}

#[test]
fn aec_pipeline_resyncs_promptly_after_dac_drop() {
    let (mut pipe, ref_feed) = aec::Pipeline::new(aec::TAIL);
    let encode_ch8_block = |ch8: &[i16]| -> (Vec<[i16; 9]>, Vec<u8>) {
        let mut decoded = Vec::with_capacity(ch8.len());
        let mut bytes = Vec::with_capacity(ch8.len() * CAP_CHANNELS * 3);
        for (i, &r) in ch8.iter().enumerate() {
            let mut row = [0i16; 9];
            row[8] = r;
            decoded.push(row);
            for c in 0..CAP_CHANNELS {
                let v: i32 = if c == 8 { (r as i32) << 8 } else { 0 };
                bytes.push((v & 0xff) as u8);
                bytes.push(((v >> 8) & 0xff) as u8);
                bytes.push(((v >> 16) & 0xff) as u8);
            }
            let _ = i;
        }
        (decoded, bytes)
    };

    // 1. Feed 160 ms of non-silent 48 kHz stereo audio and lock on the first 80 ms block.
    let mut rng = 424242u64;
    let mut next = move || {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((rng >> 33) as i32 % 8000) as i16
    };
    let mut stereo1 = Vec::with_capacity(2 * 7680);
    let mut ch8_1 = Vec::with_capacity(UPLINK_FRAMES as usize);
    for i in 0..7680usize {
        let v = next();
        stereo1.push(v);
        stereo1.push(v);
        if i >= 300 && i % 3 == 0 && ch8_1.len() < UPLINK_FRAMES as usize {
            ch8_1.push(v);
        }
    }
    ref_feed.feed(&stereo1);
    let (dec1, bytes1) = encode_ch8_block(&ch8_1);
    let mut fused = vec![0i16; UPLINK_FRAMES as usize];
    let (_cleaned, locked1) = pipe.process(&dec1, &bytes1, &mut fused);
    assert!(locked1, "first block must lock dealias");
    assert!(pipe.dealias_stats().locked);

    // 2. Simulate Stop -> hardware playback_flush() -> on_dac_drop() + silence blocks.
    ref_feed.on_dac_drop();
    let (dec_z, bytes_z) = encode_ch8_block(&vec![0i16; UPLINK_FRAMES as usize]);
    let (_cleaned_z, _locked_z) = pipe.process(&dec_z, &bytes_z, &mut fused);

    // 3. New play starts: feed new 48 kHz stereo audio and process its first 80 ms block.
    let mut stereo2 = Vec::with_capacity(2 * 7680);
    let mut ch8_2 = Vec::with_capacity(UPLINK_FRAMES as usize);
    for i in 0..7680usize {
        let v = next();
        stereo2.push(v);
        stereo2.push(v);
        if i >= 300 && i % 3 == 0 && ch8_2.len() < UPLINK_FRAMES as usize {
            ch8_2.push(v);
        }
    }
    ref_feed.feed(&stereo2);
    let (dec2, bytes2) = encode_ch8_block(&ch8_2);
    let (_cleaned2, locked2) = pipe.process(&dec2, &bytes2, &mut fused);
    assert!(
        locked2,
        "must relock on the very first 80ms (<100ms) non-zero block after Stop"
    );
    let st = pipe.dealias_stats();
    assert!(st.locked);
    assert_eq!(st.losses, 0);
    assert_eq!(st.backoffs, 0);
}

#[test]
fn resolve_production_device_id_rejects_missing_and_accepts_explicit_or_file() {
    let dir = std::env::temp_dir().join(format!("hm-gw-devid-{}-{}", std::process::id(), now_ms()));
    let _ = std::fs::create_dir_all(&dir);

    // Missing both flag and file -> Err (never falls back to "dot2-d").
    let err = resolve_production_device_id(None, &dir).unwrap_err();
    assert!(
        err.contains("missing device identity"),
        "unexpected error: {err}"
    );

    // Empty explicit flag -> Err.
    assert!(resolve_production_device_id(Some("   "), &dir).is_err());

    // Empty file -> Err.
    std::fs::write(dir.join("device-id"), " \n").unwrap();
    assert!(resolve_production_device_id(None, &dir).is_err());

    // Valid file -> Ok.
    std::fs::write(dir.join("device-id"), "dot2-c\n").unwrap();
    assert_eq!(resolve_production_device_id(None, &dir).unwrap(), "dot2-c");

    // Explicit flag overrides file.
    assert_eq!(
        resolve_production_device_id(Some("dot2-e"), &dir).unwrap(),
        "dot2-e"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unit_profiles_all_five_dot2_units_have_distinct_acoustics() {
    let units = ["dot2-a", "dot2-b", "dot2-c", "dot2-d", "dot2-e"];
    let mut loaded = Vec::new();
    for &u in &units {
        let dir = std::env::temp_dir().join(format!("hm-gw-prof-{u}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let prof = profile::DeviceProfile::load_from_dir(&dir, u);
        assert_eq!(prof.unit_id, u);
        assert_eq!(prof.model, "AEOBC");
        assert_eq!(prof.led_count, leds::RING_LEDS);
        loaded.push(prof);
        let _ = std::fs::remove_dir_all(&dir);
    }

    for i in 0..loaded.len() {
        for j in (i + 1)..loaded.len() {
            let a = &loaded[i];
            let b = &loaded[j];
            assert!(
                (a.volume_cap - b.volume_cap).abs() > 1e-6,
                "{} and {} must have distinct volume_cap",
                a.unit_id,
                b.unit_id
            );
            assert!(
                (a.mix_gain - b.mix_gain).abs() > 1e-6,
                "{} and {} must have distinct mix_gain",
                a.unit_id,
                b.unit_id
            );
            assert!(
                (a.vad_threshold - b.vad_threshold).abs() > 1e-6,
                "{} and {} must have distinct vad_threshold",
                a.unit_id,
                b.unit_id
            );
        }
    }
}

#[test]
fn unit_profiles_drive_runtime_playback_mix_gain_and_vad_threshold() {
    let mut cfg_d = FirmwareConfig::default();
    cfg_d.apply_profile(
        profile::DeviceProfile::for_unit("dot2-d"),
        false,
        false,
        false,
    );

    let mut cfg_c = FirmwareConfig::default();
    cfg_c.apply_profile(
        profile::DeviceProfile::for_unit("dot2-c"),
        false,
        false,
        false,
    );

    // Different unit profiles produce distinct runtime volume_cap, barge_in_db (VAD threshold),
    // and effective playback gain at the same volume level.
    assert!(
        (cfg_d.volume_cap - cfg_c.volume_cap).abs() > 1e-6,
        "volume_cap must differ between dot2-d and dot2-c"
    );
    assert!(
        (cfg_d.barge_in_db - cfg_c.barge_in_db).abs() > 1e-6,
        "barge_in_db (vad_threshold) must differ between dot2-d ({}) and dot2-c ({})",
        cfg_d.barge_in_db,
        cfg_c.barge_in_db
    );
    assert_eq!(cfg_d.barge_in_db, cfg_d.profile.vad_threshold);
    assert_eq!(cfg_c.barge_in_db, cfg_c.profile.vad_threshold);

    let gain_d = playback::effective_playback_gain(100, cfg_d.profile.mix_gain);
    let gain_c = playback::effective_playback_gain(100, cfg_c.profile.mix_gain);
    assert!(
        (gain_d - gain_c).abs() > 1e-5,
        "effective playback gain must differ between dot2-d ({gain_d}) and dot2-c ({gain_c})"
    );
    assert!(
        gain_c < gain_d,
        "dot2-c (mix_gain=0.80) must scale lower than dot2-d (mix_gain=1.00)"
    );

    // Explicit CLI --barge-in-db / --volume-cap overrides are preserved when applying profile.
    let mut cfg_override = FirmwareConfig {
        volume_cap: 0.72,
        barge_in_db: -48.5,
        ..FirmwareConfig::default()
    };
    cfg_override.apply_profile(
        profile::DeviceProfile::for_unit("dot2-b"),
        false,
        true,
        true,
    );
    assert!((cfg_override.volume_cap - 0.72).abs() < 1e-6);
    assert!((cfg_override.barge_in_db - (-48.5)).abs() < 1e-6);
}

#[test]
fn mic_gain_metadata_is_preserved_but_keyword_wire_rms_stays_raw() {
    for unit in ["dot2-a", "dot2-b", "dot2-c", "dot2-d", "dot2-e"] {
        let prof = profile::DeviceProfile::for_unit(unit);
        assert!(
            prof.mic_gain_offset_db.abs() < 1e-6,
            "{unit} preset must default mic_gain_offset_db to 0.0"
        );
    }

    let dir = std::env::temp_dir().join(format!("hm-gw-mic-offset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // profile.json without mic_gain_offset_db deserializes with default 0.0.
    let mut base = profile::DeviceProfile::for_unit("dot2-a");
    let mut v = serde_json::to_value(&base).unwrap();
    v.as_object_mut().unwrap().remove("mic_gain_offset_db");
    std::fs::write(dir.join("profile.json"), serde_json::to_string(&v).unwrap()).unwrap();
    let loaded_default = profile::DeviceProfile::load_from_dir(&dir, "dot2-a");
    assert!((loaded_default.mic_gain_offset_db - 0.0).abs() < 1e-6);

    // profile.json with explicit mic_gain_offset_db overrides the preset.
    base.mic_gain_offset_db = 3.5;
    std::fs::write(
        dir.join("profile.json"),
        serde_json::to_string(&base).unwrap(),
    )
    .unwrap();
    let loaded_override = profile::DeviceProfile::load_from_dir(&dir, "dot2-a");
    assert!((loaded_override.mic_gain_offset_db - 3.5).abs() < 1e-6);

    // A nonzero stored offset is metadata, not permission to correct wire RMS.
    // The capture level window and KWS frame share the raw post-AEC level.
    let mut levels = LevelWindow::default();
    assert_eq!(levels.peak_over_ms(810), None);
    levels.push(-31.5);
    let raw = levels.peak_over_ms(810);
    assert_eq!(raw, Some(-31.5));

    let status: kws::SharedStatus = Arc::new(Mutex::new(kws::KwsStatus::default()));
    let frame = kws::north_keyword(
        &loaded_override.unit_id,
        "你好高达",
        2720,
        1_700_000_000_000,
        &status,
        false,
        raw,
        Some(90),
    );
    match frame {
        GatewayFrame::Keyword { rms_dbfs, .. } => {
            assert_eq!(rms_dbfs, Some(-31.5));
        }
        other => panic!("unexpected frame: {other:?}"),
    }

    // Wake phrase window aligns to fed_ms + lag_ms and clamps to [LEVEL_BLOCK_MS, fed_ms].
    assert_eq!(kws::wake_phrase_window_ms(2720, None), 720);
    assert_eq!(kws::wake_phrase_window_ms(2720, Some(90)), 810);
    assert_eq!(kws::wake_phrase_window_ms(240, None), 240);
    assert_eq!(kws::wake_phrase_window_ms(40, None), LEVEL_BLOCK_MS);

    let _ = std::fs::remove_dir_all(&dir);
}
