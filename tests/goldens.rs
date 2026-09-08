//! Pipeline smoke test (all platforms) and golden-image comparison.
//! Goldens are font-rendering-sensitive and only valid for PNGs produced in
//! the debian:trixie container, so the comparison runs only when
//! PLACARD_GOLDENS=1 (set by CI), not on dev machines with different font
//! stacks. Regenerate deliberately with `make goldens`.

use std::path::{Path, PathBuf};
use std::process::Command;

use gstreamer as gst;
use gstreamer::prelude::*;

const TOLERANCE: u8 = 2; // per-channel, out of 255
const MAX_DIFF_FRACTION: f64 = 0.001; // 0.1 % of pixels may exceed it

fn repo(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn snapshot(fixture: &Path, out: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_placard"))
        .arg("--config")
        .arg(repo("packaging/config.toml.example"))
        .arg("--snapshot")
        .arg(fixture)
        .arg(out)
        .status()
        .expect("running placard --snapshot");
    assert!(
        status.success(),
        "--snapshot failed for {}",
        fixture.display()
    );
}

/// Decode a PNG to raw RGBA via GStreamer, which the tests already depend
/// on, rather than pulling in an image crate for this one job. Dimensions
/// come from the PNG IHDR; pixels from a pngdec ! filesink dump.
fn decode_rgba(png: &Path) -> (u32, u32, Vec<u8>) {
    let header = std::fs::read(png).expect("reading png");
    assert!(header.len() > 24 && &header[1..4] == b"PNG", "not a png");
    let width = u32::from_be_bytes(header[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(header[20..24].try_into().unwrap());

    gst::init().unwrap();
    let raw = png.with_extension("rgba");
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! pngdec ! videoconvert ! video/x-raw,format=RGBA ! filesink location=\"{}\"",
        png.display(),
        raw.display()
    ))
    .expect("building decode pipeline");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    if let Some(msg) = msg
        && let gst::MessageView::Error(err) = msg.view()
    {
        panic!("decode failed: {}", err.error());
    }

    let pixels = std::fs::read(&raw).expect("reading raw dump");
    let _ = std::fs::remove_file(&raw);
    assert_eq!(
        pixels.len(),
        (width * height * 4) as usize,
        "unexpected raw frame size for {}",
        png.display()
    );
    (width, height, pixels)
}

#[test]
fn pipeline_smoke_one_frame() {
    let out = std::env::temp_dir().join(format!("placard-smoke-{}.png", std::process::id()));
    snapshot(&repo("tests/scenes/text_short.json"), &out);
    let (w, h, pixels) = decode_rgba(&out);
    let _ = std::fs::remove_file(&out);
    assert_eq!((w, h), (1920, 1080));
    let (px, _) = pixels.as_chunks::<4>();
    assert!(
        px.iter().any(|p| p != &px[0]),
        "frame is a single flat colour — text and clock layers missing"
    );
}

#[test]
fn golden_images_match() {
    if std::env::var_os("PLACARD_GOLDENS").is_none() {
        eprintln!("skipping golden comparison: PLACARD_GOLDENS not set (Linux container only)");
        return;
    }
    let scenes = std::fs::read_dir(repo("tests/scenes")).unwrap();
    let mut compared = 0;
    for entry in scenes {
        let fixture = entry.unwrap().path();
        if fixture.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = fixture.file_stem().unwrap().to_string_lossy().to_string();
        let golden = repo(&format!("tests/golden/{name}.png"));
        assert!(
            golden.exists(),
            "no golden for {name}; run `make goldens` and commit the result"
        );

        let out = std::env::temp_dir().join(format!("placard-golden-{name}.png"));
        snapshot(&fixture, &out);
        let (gw, gh, expected) = decode_rgba(&golden);
        let (aw, ah, actual) = decode_rgba(&out);
        let _ = std::fs::remove_file(&out);
        assert_eq!((gw, gh), (aw, ah), "{name}: dimensions differ");

        let differing = expected
            .iter()
            .zip(&actual)
            .filter(|(e, a)| e.abs_diff(**a) > TOLERANCE)
            .count();
        // Channel-level count over 4·w·h is stricter than pixel-level; fine.
        let fraction = differing as f64 / expected.len() as f64;
        assert!(
            fraction <= MAX_DIFF_FRACTION,
            "{name}: {:.3} % of samples differ by more than {TOLERANCE}/255",
            fraction * 100.0
        );
        compared += 1;
    }
    assert!(
        compared >= 7,
        "expected at least the seven standard fixtures, compared {compared}"
    );
}
