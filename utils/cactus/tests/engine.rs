//! Real-engine tests, off the default gate. They need the pinned Mirage
//! weights: `mise run cactus:smoke`, or set `CACTUS_MIRAGE_WEIGHTS` and run
//! `cargo test -p cactus --test engine -- --ignored --test-threads=1`.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a missing weights file or a failing engine call fails the test"
)]

use std::time::Instant;

use cactus::{Engine, Error, PackedRgb, Request};

fn weights() -> Vec<u8> {
    let path =
        std::env::var("CACTUS_MIRAGE_WEIGHTS").expect("CACTUS_MIRAGE_WEIGHTS names mirage.cact");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading {path}: {err}"))
}

/// A 160x120 frame: a bright square on a dark gradient.
fn image() -> Vec<u8> {
    let (width, height) = (160usize, 120usize);
    let mut data = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            let inside = (50..110).contains(&x) && (30..90).contains(&y);
            let value = if inside {
                230
            } else {
                u8::try_from(x / 2).unwrap_or(0)
            };
            data.extend_from_slice(&[value, value, if inside { 40 } else { value }]);
        }
    }
    data
}

fn loaded() -> Engine {
    let mut engine = Engine::lock();
    engine.load_vision(&weights()).expect("Mirage loads");
    engine
}

#[test]
#[ignore = "needs the Mirage weights"]
fn describes_an_image() {
    let mut engine = loaded();
    assert!(engine.models().vision());
    let pixels = image();
    let frame = PackedRgb::new(&pixels, 160, 120).expect("image");
    let started = Instant::now();
    let description = engine
        .describe(
            frame,
            Request {
                options: r#"{"sections":["objects","text"],"fields":["bbox","score"]}"#,
                ..Request::default()
            },
        )
        .expect("describe");
    eprintln!(
        "describe: {:?}, {} tokens, {} bytes: {}",
        started.elapsed(),
        description.tokens,
        description.json.len(),
        description.json
    );
    let value: serde_json::Value = serde_json::from_str(&description.json).expect("JSON");
    assert!(value.get("objects").is_some(), "{value}");
    assert_eq!(description.grew_to, None);
}

#[test]
#[ignore = "needs the Mirage weights"]
fn other_vision_weights_cannot_replace_loaded_ones() {
    let mut engine = loaded();
    engine
        .load_vision(&weights())
        .expect("the same weights load again");
    assert_eq!(
        engine.vision_weights(),
        Some(cactus::sha256_hex(&weights()).as_str())
    );
    // Same manifest, different bytes: other weights as far as the slot knows.
    let mut other = weights();
    if let Some(last) = other.last_mut() {
        *last ^= 0xff;
    }
    let error = engine.load_vision(&other).expect_err("other weights");
    assert!(matches!(error, Error::ModelInUse { .. }), "{error}");
}

#[test]
#[ignore = "needs the Mirage weights"]
fn threads_after_loading_are_too_late() {
    // No other test sets threads, so the first explicit value comes after
    // Mirage is loaded and has no effect.
    let mut engine = loaded();
    assert_eq!(
        engine.set_threads(3),
        cactus::Threads::TooLate { requested: 3 }
    );
}

#[test]
#[ignore = "needs the Mirage weights"]
fn reports_invalid_options() {
    let mut engine = loaded();
    let pixels = image();
    let frame = PackedRgb::new(&pixels, 160, 120).expect("image");
    let error = engine
        .describe(
            frame,
            Request {
                options: r#"{"sections":["nonsense"]}"#,
                ..Request::default()
            },
        )
        .expect_err("invalid section");
    eprintln!("invalid options: {error}");
    assert!(matches!(error, Error::Engine { .. }), "{error}");
}

#[test]
#[ignore = "needs the Mirage weights"]
fn grows_a_small_output_buffer() {
    let mut engine = loaded();
    engine.set_output_capacity_for_tests(32);
    let pixels = image();
    let frame = PackedRgb::new(&pixels, 160, 120).expect("image");
    let description = engine
        .describe(
            frame,
            Request {
                options: r#"{"sections":["objects"],"fields":["bbox"]}"#,
                ..Request::default()
            },
        )
        .expect("describe after growing");
    eprintln!("grew to {:?}", description.grew_to);
    assert!(
        description
            .grew_to
            .is_some_and(|size| size > description.json.len()),
        "{:?}",
        description.grew_to
    );
    serde_json::from_str::<serde_json::Value>(&description.json).expect("JSON");
}
