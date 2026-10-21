use std::process::Command;

#[test]
fn inventory_never_claims_device_validation() {
    let output = Command::new(env!("CARGO_BIN_EXE_frame-probe"))
        .arg("inventory")
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["kind"], "inventory_only");
    assert_eq!(report["frame_hardware_decoding_verified"], false);
    assert!(report["vulkan"].is_object());
}

#[cfg(feature = "decode")]
#[test]
fn decode_rejects_network_urls_before_opening() {
    let output = Command::new(env!("CARGO_BIN_EXE_frame-probe"))
        .args([
            "decode",
            "smb://example.invalid/private/movie.mp4",
            "--backend",
            "vulkan",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("existing local file"));
}

#[cfg(feature = "decode")]
#[test]
fn decode_rejects_zero_frames() {
    let output = Command::new(env!("CARGO_BIN_EXE_frame-probe"))
        .args([
            "decode",
            "unused.mp4",
            "--backend",
            "vulkan",
            "--frames",
            "0",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[cfg(feature = "decode")]
#[test]
fn invalid_media_cannot_pass() {
    let output = Command::new(env!("CARGO_BIN_EXE_frame-probe"))
        .args([
            "decode",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            "--backend",
            "vulkan",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sample_decoded"], false);
    assert_eq!(report["frames"], 0);
    assert!(report["error"].is_string());
}

/// Opening the wrong file must produce an error message, never a crash (panic = 101).
#[cfg(feature = "decode")]
#[test]
fn player_survives_bad_files() {
    let dir = std::env::temp_dir().join(format!("jv-bad-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty.mp4", Vec::new()),
        ("text.mkv", b"definitely not a video\n".repeat(100)),
        (
            "random.mp4",
            (0..65536u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                .collect(),
        ),
        (
            "fake-header.mp4",
            [b"\0\0\0\x18ftypisom".as_slice(), &[0xAB; 4096]].concat(),
        ),
    ];
    // Truncated and corrupted copies of a real clip, when fixtures exist.
    let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/hevc-main10-720p.mp4");
    if let Ok(real) = std::fs::read(sample) {
        cases.push(("truncated.mp4", real[..real.len() / 3].to_vec()));
        let mut corrupt = real.clone();
        for i in (corrupt.len() / 4..corrupt.len()).step_by(97) {
            corrupt[i] ^= 0x5A;
        }
        cases.push(("corrupt.mp4", corrupt));
    }
    for (name, bytes) in cases {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        for args in [
            vec!["info"],
            vec!["bench", "--frames", "60", "--hw", "none"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_just-video"))
                .args(&args)
                .arg(&path)
                .output()
                .unwrap();
            let code = output.status.code();
            assert!(
                code.is_some() && code != Some(101),
                "{name} {args:?} crashed: {output:?}"
            );
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 10-bit video must never reach the Steam Frame hardware decoder (its firmware
/// crashes); asking for V4L2 decodes it in software with an explanation instead.
#[cfg(feature = "decode")]
#[test]
fn ten_bit_never_uses_v4l2() {
    let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/hevc-main10-720p.mp4");
    if !std::path::Path::new(sample).exists() {
        eprintln!("skipping: run scripts/make-samples.sh");
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_just-video"))
        .args([
            "--platform",
            "steam-frame",
            "bench",
            sample,
            "--hw",
            "v4l2",
            "--frames",
            "10",
        ])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["decode"]["hardware_frames"], 0);
    assert_ne!(report["decode"]["decoder"], "hevc_v4l2m2m");
    assert!(
        report["decode"]["note"]
            .as_str()
            .unwrap()
            .contains("8-bit only")
    );
    assert_eq!(report["playability"]["verdict"], "software");
}

/// The frame-by-frame API yields complete planes with increasing timestamps.
#[cfg(feature = "decode")]
#[test]
fn decoder_yields_frames_with_planes_and_pts() {
    use just_video::media::{Media, PlaneLayout};
    for (name, bits) in [("h264-720p.mp4", 8), ("hevc-main10-720p.mp4", 10)] {
        let path = format!("{}/samples/{name}", env!("CARGO_MANIFEST_DIR"));
        let Ok(file) = std::fs::File::open(&path) else {
            eprintln!("skipping {name}: run scripts/make-samples.sh");
            continue;
        };
        let media = Media::open(name, file).unwrap();
        let mut decoder = media.into_decoder(None, true, "").unwrap();
        let mut last = -1.0;
        let mut count = 0;
        while let Some(frame) = decoder.next_frame().unwrap() {
            assert_eq!(
                (frame.width(), frame.height(), frame.bits()),
                (1280, 720, bits)
            );
            assert_eq!(frame.layout(), PlaneLayout::Planar);
            for plane in 0..frame.plane_count() {
                let (w, h, c) = frame.plane_size(plane);
                let rows: Vec<&[u8]> = frame.rows(plane).collect();
                assert_eq!(rows.len() as u32, h);
                assert!(
                    rows.iter()
                        .all(|r| r.len() == (w * c) as usize * frame.bytes_per_sample())
                );
            }
            let pts = frame.pts().unwrap();
            assert!(pts > last, "{name}: pts {pts} after {last}");
            last = pts;
            count += 1;
        }
        assert_eq!(count, 60, "{name}");
        decoder.seek(1.0).unwrap();
        assert!(decoder.next_frame().unwrap().is_some());
    }
}

/// After the hardware decoder refuses to restart (seen on the Steam Frame with
/// 8K 60 fps video after a seek), the player opens the decoder again and
/// seeks. The CPU decoder takes the same path, so a short generated clip with
/// keyframes 4 s apart (needs the ffmpeg command) stands in for it.
#[cfg(feature = "decode")]
#[test]
fn decoder_reset_then_seek_resumes_at_the_target() {
    use just_video::media::Media;
    let dir = std::env::temp_dir().join(format!("just-video-reset-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("clip.mp4");
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=160x96:rate=30:duration=12")
        .args(["-c:v", "libx264", "-g", "120", "-keyint_min", "120"])
        .args(["-sc_threshold", "0", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipping: needs the ffmpeg command with libx264");
        return;
    }
    let media = Media::open("clip.mp4", std::fs::File::open(&clip).unwrap()).unwrap();
    let mut decoder = media.into_decoder(None, true, "").unwrap();
    for _ in 0..10 {
        decoder.next_frame().unwrap().unwrap();
    }
    decoder.reset().unwrap();
    assert!(!decoder.hardware());
    // Keyframes are at 0, 4 and 8 s: a jump to 6 s lands on the one at 4 s.
    decoder.seek(6.0).unwrap();
    let first = decoder.next_frame().unwrap().unwrap().pts().unwrap();
    assert!((first - 4.0).abs() < 0.05, "first picture at {first}");
    let mut last = first;
    let mut count = 1;
    while let Some(frame) = decoder.next_frame().unwrap() {
        let pts = frame.pts().unwrap();
        assert!(pts > last, "pts {pts} after {last}");
        last = pts;
        count += 1;
    }
    assert_eq!(count, 240, "4 s to the end at 30 fps");
    // Resetting at the end and seeking back works too.
    decoder.reset().unwrap();
    decoder.seek(0.0).unwrap();
    assert!(decoder.next_frame().unwrap().is_some());
    std::fs::remove_dir_all(&dir).ok();
}
