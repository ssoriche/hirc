//! Offscreen snapshot smoke test. It runs the real binary, because AppKit and
//! Metal want the process main thread, which a test thread is not. Ignored by
//! default: it needs a Metal device.
//!
//! `cargo test -p hirc --features snapshot --test offscreen -- --ignored`
#![cfg(feature = "snapshot")]

use std::process::Command;

#[test]
#[ignore = "needs a Metal device; run with --ignored"]
fn demo_fixture_renders_a_real_image() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/demo.toml");
    let out = std::env::temp_dir().join(format!("hirc-offscreen-smoke-{}.png", std::process::id()));
    let run = Command::new(env!("CARGO_BIN_EXE_hirc"))
        .env("HIRC_OFFSCREEN", "1")
        .env("HIRC_FIXTURE", fixture)
        .env("HIRC_SIZE", "900x600")
        .env("HIRC_SCRIPT", "window:2")
        .env("HIRC_SCREENSHOT", &out)
        .output()
        .expect("run hirc");
    assert!(run.status.success(), "hirc failed: {}", String::from_utf8_lossy(&run.stderr));
    let img = image::open(&out).expect("png").to_rgba8();
    let _ = std::fs::remove_file(&out);
    assert!(img.width() > 1 && img.height() > 1, "{}x{}", img.width(), img.height());
    let first = *img.get_pixel(0, 0);
    assert!(img.pixels().any(|p| *p != first), "image is a single colour");
}
