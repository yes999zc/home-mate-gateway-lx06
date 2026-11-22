use super::*;

#[test]
fn gateway_frames_round_trip() {
    let frames = vec![
        GatewayFrame::Hello {
            protocol: 0,
            min_protocol: 0,
            gateway: "home-mate-device-gateway 0.0.2".into(),
            gateway_version: "0.0.2".into(),
            aec_tail_ms: aec_tail_ms(true, crate::firmware::aec::TAIL),
            profile_version: Some(PROFILE_VERSION.into()),
            supported_versions: vec![],
            devices: vec![HelloDevice {
                stable_id: "dot2-d".into(),
                model: "AEOBC".into(),
                firmware: Some("fireos-5.5.5.4".into()),
                capabilities: vec![
                    "wake_event".into(),
                    "text_in".into(),
                    "audio_out".into(),
                    "volume".into(),
                    "stop".into(),
                ],
                audio_profile: "fireos_builtin_speaker".into(),
                wifi_ssid: None,
            }],
        },
        GatewayFrame::Wake {
            device: "dot2-d".into(),
            ts: 1762500001123,
            seq: 481,
        },
        GatewayFrame::Utterance {
            device: "dot2-d".into(),
            ts: 1762500002000,
            seq: 482,
            source: "text".into(),
            text: "讲个睡前故事".into(),
        },
        GatewayFrame::PlayReceipt {
            task_id: "T01".into(),
            stage: "device_confirmed".into(),
            detail: Some("playing voice channel".into()),
            ts: 1762500003000,
        },
        GatewayFrame::PlayRejected {
            task_id: "T01-rej".into(),
            device: "dot2-d".into(),
            reason: "wifi_provisioning".into(),
            ts: 1762500003500,
        },
        GatewayFrame::StopReceipt {
            task_id: "T01".into(),
            stage: "device_confirmed".into(),
            played_ms: Some(1400),
            consumed_ms: Some(1260),
            progress_basis: Some("hw_delay".into()),
            detail: None,
        },
        GatewayFrame::VolumeReceipt {
            task_id: "T02".into(),
            effective: 0.6,
            unit: "linear".into(),
            stage: "device_confirmed".into(),
            device: Some("dot2-d".into()),
        },
        GatewayFrame::ModeReport {
            device: "dot2-d".into(),
            duplex: DuplexState {
                capability: "half".into(),
                requested: "full".into(),
                negotiated: "half".into(),
                effective: "half".into(),
            },
            aec: AecState {
                owner: "device".into(),
                reference: "available".into(),
            },
        },
        GatewayFrame::PlayFinished {
            device: "dot2-d".into(),
            task_id: "T01".into(),
            played_ms: 1400,
            consumed_ms: Some(1230),
            progress_basis: Some("hw_delay".into()),
            ts: 1762500004400,
        },
        // v0.2.4 (D/T4): reflex fact + spotter lifecycle.
        GatewayFrame::Keyword {
            device: "dot2-d".into(),
            ts: 1762500005000,
            seq: 77,
            keyword: "你好高达".into(),
            fed_ms: Some(2720),
            self_heard: false,
            rms_dbfs: Some(-31.5),
            lag_ms: Some(90),
            bundle_id: Some("b999db10".into()),
            kws_generation: Some(1),
        },
        GatewayFrame::KwsReport {
            device: "dot2-d".into(),
            state: "running".into(),
            detail: "ready in 6797ms".into(),
            restarts: 2,
            detections: 5,
            guarded: 1,
            epoch: 1762500000000,
            loaded_hash: "b999db10deadbeef".into(),
            loaded_ms: 1762500000500,
            local_exit: 1,
            threshold: 0.25,
            score: 3.0,
        },
        // v0.2.5 (D/T5): near-end speech fact with onset pre-roll.
        GatewayFrame::BargeIn {
            device: "dot2-d".into(),
            ts: 1762500006100,
            seq: 78,
            pcm_b64: "AAAA".into(),
            sample_rate: 16000,
            channels: 1,
            corr: 0.31,
            energy_db: -35.2,
            ref_db: -20.0,
            residual_db: 15.2,
            ref_locked: true,
        },
    ];
    for f in frames {
        let line = encode_gateway(&f).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(&decode_gateway(&line).unwrap(), &f, "round trip: {line}");
    }
}

#[test]
fn core_frames_round_trip() {
    let frames = vec![
        CoreFrame::HelloAck {
            protocol: 0,
            min_protocol: 0,
            core_version: "0.0.1".into(),
            supported_versions: vec![],
            capabilities: vec![],
            generation: 12,
        },
        CoreFrame::Play {
            task_id: "T01".into(),
            device: "dot2-d".into(),
            content_ref: "tts:abc123".into(),
            audio: AudioMeta {
                format: "wav".into(),
                sample_rate: 16000,
                channels: 1,
            },
            audio_b64: Some("AAAA".into()),
            channel: Some("voice".into()),
            text: Some("你好".into()),
            simulated_ms: None,
            barge_in: Some("stop_current".into()),
        },
        CoreFrame::Play {
            task_id: "T09".into(),
            device: "dot2-d".into(),
            content_ref: "mocklib:t01".into(),
            audio: AudioMeta {
                format: "wav".into(),
                sample_rate: 16000,
                channels: 1,
            },
            audio_b64: None,
            channel: Some("media".into()),
            text: Some("晚风信笺 — 林晚".into()),
            simulated_ms: Some(30000),
            barge_in: Some("stop_current".into()),
        },
        CoreFrame::Stop {
            task_id: "T09".into(),
            flush: true,
        },
        CoreFrame::VolumeSet {
            task_id: "T02".into(),
            device: "dot2-d".into(),
            value: 0.6,
            unit: "linear".into(),
        },
        CoreFrame::ModeSet {
            device: "dot2-d".into(),
            duplex_requested: "full".into(),
        },
        CoreFrame::AudioEq {
            device: "dot2-d".into(),
            curve: "echomuse".into(),
            bands: vec![
                EqBand {
                    freq: 175.0,
                    gain_db: -6.0,
                    q: 0.6,
                },
                EqBand {
                    freq: 3500.0,
                    gain_db: 4.0,
                    q: 1.2,
                },
                EqBand {
                    freq: 8000.0,
                    gain_db: 2.0,
                    q: 1.0,
                },
            ],
        },
        CoreFrame::LedConfig {
            device: "dot2-d".into(),
            night_enabled: true,
            night_start_minute: 1320,
            night_end_minute: 420,
            night_brightness_pct: 15,
            orientation_rotation: 3,
            orientation_reverse: true,
        },
    ];
    for f in frames {
        let line = encode_core(&f).unwrap();
        assert_eq!(&decode_core(&line).unwrap(), &f, "round trip: {line}");
    }
}

/// The type tag must match the contract's names exactly.
#[test]
fn wire_type_tags_match_contract() {
    assert!(encode_gateway(&GatewayFrame::Hello {
        protocol: 0,
        min_protocol: 0,
        gateway: "x".into(),
        gateway_version: String::new(),
        aec_tail_ms: None,
        profile_version: None,
        supported_versions: vec![],
        devices: vec![],
    })
    .unwrap()
    .starts_with("{\"type\":\"hello\""));
    assert!(encode_core(&CoreFrame::HelloAck {
        protocol: 0,
        min_protocol: 0,
        core_version: String::new(),
        supported_versions: vec![],
        capabilities: vec![],
        generation: 1
    })
    .unwrap()
    .starts_with("{\"type\":\"hello_ack\""));
    assert!(encode_gateway(&GatewayFrame::PlayReceipt {
        task_id: "T".into(),
        stage: "sent".into(),
        detail: None,
        ts: 1,
    })
    .unwrap()
    .starts_with("{\"type\":\"play_receipt\""));
    assert!(encode_core(&CoreFrame::ModeSet {
        device: "d".into(),
        duplex_requested: "half".into()
    })
    .unwrap()
    .starts_with("{\"type\":\"mode_set\""));
    assert!(encode_gateway(&GatewayFrame::PlayFinished {
        device: "d".into(),
        task_id: "T".into(),
        played_ms: 5,
        consumed_ms: None,
        progress_basis: Some("written".into()),
        ts: 1,
    })
    .unwrap()
    .starts_with("{\"type\":\"play_finished\""));
}

/// Unknown frame types must fail closed, not silently drop.
#[test]
fn unknown_type_fails() {
    assert!(decode_gateway("{\"type\":\"teleport\"}").is_err());
    assert!(decode_core("{\"type\":\"teleport\"}").is_err());
}

/// Contract golden sample: the hello example from contracts-v0.md §握手
/// parses (field names are the cross-repo agreement).
#[test]
fn contract_hello_sample_parses() {
    let sample = r#"{"type":"hello","protocol":0,"gateway":"home-mate-device-gateway 0.0.1",
          "devices":[{"stable_id":"dot2-d","model":"AEOBC",
             "capabilities":["wake_event","text_in","audio_out","volume","stop"],
             "audio_profile":"fireos_builtin_speaker"}]}"#;
    let f = decode_gateway(sample).unwrap();
    match f {
        GatewayFrame::Hello { devices, .. } => {
            assert_eq!(devices[0].stable_id, "dot2-d");
            assert_eq!(devices[0].capabilities.len(), 5);
            assert!(devices[0].firmware.is_none());
        }
        _ => panic!("wrong frame"),
    }
}

/// v1 mechanism (#11): the NEW frames decode, including from a peer that
/// omits the optional fields (the mixed-window shapes).
#[test]
fn v1_frames_round_trip_and_old_shaped_frames_decode() {
    let mute_set = CoreFrame::MuteSet {
        task_id: "M1".into(),
        device: "dot2-d".into(),
        muted: false,
    };
    let line = encode_core(&mute_set).unwrap();
    assert!(line.starts_with("{\"type\":\"mute_set\""), "{line}");
    assert_eq!(decode_core(&line).unwrap(), mute_set);

    let reject = CoreFrame::HelloReject {
        protocol: 0,
        min_protocol: 0,
        core_version: "0.0.1".into(),
        reason: "protocol_no_intersection: peer accepts [1,1], this side accepts [0,0]".into(),
    };
    assert_eq!(decode_core(&encode_core(&reject).unwrap()).unwrap(), reject);

    let proto_err = GatewayFrame::ProtocolError {
        protocol: 1,
        min_protocol: 0,
        gateway_version: "0.0.53".into(),
        reason: "capability_missing:wifi".into(),
    };
    assert_eq!(
        decode_gateway(&encode_gateway(&proto_err).unwrap()).unwrap(),
        proto_err
    );

    // An old-shaped MuteState (no task_id) still decodes — the field is
    // optional so release-67-era facts keep flowing.
    let old_mute: GatewayFrame =
            decode_gateway(r#"{"type":"mute_state","device":"dot2-d","ts":1,"seq":2,"muted":true,"source":"device_key"}"#)
                .unwrap();
    match old_mute {
        GatewayFrame::MuteState { task_id, .. } => assert_eq!(task_id, None),
        f => panic!("wrong frame: {f:?}"),
    }
}

/// v1 mechanism (#11): compatibility is interval intersection. The naive
/// "peer version inside my range" checked on both sides degenerates into
/// "highest versions equal" — the exact failure that would have blocked
/// the 0→1 bump inside the mixed activation window.
#[test]
fn protocol_intervals_intersect_not_mirror() {
    let core = (0u32, 1u32); // release B
    let firmware_old = (0u32, 0u32); // this release
    assert_eq!(protocol_operating_version(firmware_old, core), Some(0));
    assert_eq!(protocol_operating_version(core, firmware_old), Some(0));
    assert_eq!(protocol_operating_version((0, 0), (0, 0)), Some(0));
    assert_eq!(protocol_operating_version((0, 1), (1, 1)), Some(1));
    // Disjoint intervals reject in BOTH directions.
    assert_eq!(protocol_operating_version((0, 1), (2, 3)), None);
    assert_eq!(protocol_operating_version((2, 3), (0, 1)), None);
    // An illegal interval (min > max) never intersects.
    assert_eq!(protocol_operating_version((1, 0), (0, 0)), None);
    assert_eq!(protocol_operating_version((0, 0), (1, 0)), None);
    let reason = protocol_no_intersection_reason((0, 0), (1, 1));
    assert!(reason.contains(REASON_PROTOCOL_NO_INTERSECTION), "{reason}");
}

#[test]
fn hello_carries_aec_tail_and_profile_version_with_backward_compat() {
    assert_eq!(aec_tail_ms(true, 2560), Some(160));
    assert_eq!(aec_tail_ms(true, 10240), Some(640));
    assert_eq!(aec_tail_ms(false, 10240), None);
    assert_eq!(PROFILE_VERSION, "dot2-array-postaec-v1");

    let hello = GatewayFrame::Hello {
        protocol: PROTOCOL_VERSION,
        min_protocol: PROTOCOL_MIN_VERSION,
        gateway: "gw".into(),
        gateway_version: "0.0.53".into(),
        aec_tail_ms: aec_tail_ms(true, crate::firmware::aec::TAIL),
        profile_version: Some(PROFILE_VERSION.into()),
        supported_versions: vec![],
        devices: vec![],
    };
    let wire = encode_gateway(&hello).unwrap();
    assert_eq!(decode_gateway(&wire).unwrap(), hello);

    // Old Hello without aec_tail_ms / profile_version still decodes cleanly.
    let legacy = r#"{"type":"hello","protocol":0,"gateway":"old","devices":[]}"#;
    match decode_gateway(legacy).unwrap() {
        GatewayFrame::Hello {
            aec_tail_ms,
            profile_version,
            ..
        } => {
            assert_eq!(aec_tail_ms, None);
            assert_eq!(profile_version, None);
        }
        other => panic!("unexpected frame: {other:?}"),
    }
}

#[test]
fn echo_health_and_near_end_presence_round_trip() {
    let eh = GatewayFrame::EchoHealth {
        device: "dot2-d".into(),
        health: EchoPathHealth::Degraded,
        degraded_ms: 480,
        reason: "ref_unlocked".into(),
    };
    let eh_wire = encode_gateway(&eh).unwrap();
    assert!(eh_wire.starts_with("{\"type\":\"echo_health\""));
    assert!(eh_wire.contains("\"health\":\"degraded\""));
    assert_eq!(decode_gateway(&eh_wire).unwrap(), eh);

    let np = GatewayFrame::NearEndPresence {
        device: "dot2-d".into(),
        ts_ms: 1762500001000,
        duration_ms: 240,
        rms_dbfs: -24.5,
        noise_floor_dbfs: -52.0,
        snr_db: 27.5,
        clipped: false,
        echo_health: EchoPathHealth::Healthy,
        profile_version: PROFILE_VERSION.into(),
    };
    let np_wire = encode_gateway(&np).unwrap();
    assert!(np_wire.starts_with("{\"type\":\"near_end_presence\""));
    assert!(np_wire.contains("\"echo_health\":\"healthy\""));
    assert_eq!(decode_gateway(&np_wire).unwrap(), np);
}

#[test]
fn protocol_v2_negotiation_and_v0_fallback_never_invents_wire1() {
    assert_eq!(
        protocol_operating_version_with_supported((0, 2), &[0, 2], (0, 2), &[0, 2]),
        Some(2)
    );
    // Legacy peer with no supported_versions field falls back to 0.
    assert_eq!(
        protocol_operating_version_with_supported((0, 2), &[0, 2], (0, 0), &[]),
        Some(0)
    );
    // Legacy [0, 1] range lands on 0, never wire 1.
    assert_eq!(
        protocol_operating_version_with_supported((0, 2), &[0, 2], (0, 1), &[]),
        Some(0)
    );
    // Explicit [1, 1] is rejected.
    assert_eq!(
        protocol_operating_version_with_supported((0, 2), &[0, 2], (1, 1), &[1]),
        None
    );
}

#[test]
fn audio_chunk_vad_fields_are_optional_and_round_trip() {
    let legacy = r#"{"type":"audio_chunk","device":"lx06","ts":1,"seq":2,"pcm_b64":"AAAA","format":"s16le","sample_rate":16000,"channels":1,"layout":"mono"}"#;
    let decoded = decode_gateway(legacy).expect("legacy audio_chunk decodes");
    assert!(matches!(
        decoded,
        GatewayFrame::AudioChunk {
            vad_speech: None,
            vad_prob: None,
            ..
        }
    ));
    let wire_legacy = encode_gateway(&decoded).unwrap();
    assert!(
        !wire_legacy.contains("vad_speech") && !wire_legacy.contains("vad_prob"),
        "None VAD fields must be omitted on the wire: {wire_legacy}"
    );

    let with_vad = GatewayFrame::AudioChunk {
        device: "lx06".into(),
        ts: 1763560000123,
        seq: 42,
        pcm_b64: "AAAA".into(),
        format: "s16le".into(),
        sample_rate: 16000,
        channels: 1,
        layout: "mono".into(),
        vad_speech: Some(true),
        vad_prob: Some(0.87),
    };
    let wire = encode_gateway(&with_vad).unwrap();
    assert!(wire.contains(r#""vad_speech":true"#));
    assert!(wire.contains(r#""vad_prob":0.87"#));
    let rt = decode_gateway(&wire).expect("vad audio_chunk decodes");
    assert_eq!(rt, with_vad);
}
