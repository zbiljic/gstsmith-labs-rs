//! Real-engine tests, off the default gate. They need the pinned Mirage
//! weights, and the thread test the Whistle weights too: `mise run
//! cactus:smoke`, or set `CACTUS_MIRAGE_WEIGHTS` and `CACTUS_WHISTLE_WEIGHTS`
//! and run `cargo test -p cactus --test engine -- --ignored
//! --test-threads=1`.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a missing weights file or a failing engine call fails the test"
)]

use std::time::Instant;

use cactus::{Engine, Error, PackedRgb, Request, Threads};

fn read(variable: &str) -> Vec<u8> {
    let path = std::env::var(variable).unwrap_or_else(|_err| panic!("{variable} is not set"));
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading {path}: {err}"))
}

fn weights() -> Vec<u8> {
    read("CACTUS_MIRAGE_WEIGHTS")
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

fn loaded_on(name: &str) -> Engine {
    let mut engine = Engine::lock(name).expect("engine");
    engine.load_vision(&weights(), 0).expect("Mirage loads");
    engine
}

fn loaded() -> Engine {
    loaded_on("")
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
        .load_vision(&weights(), 0)
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
    let error = engine.load_vision(&other, 0).expect_err("other weights");
    assert!(matches!(error, Error::ModelInUse { .. }), "{error}");
}

/// The threads of this process, as the OS counts them.
#[cfg(target_os = "linux")]
fn process_threads() -> i64 {
    let tasks = std::fs::read_dir("/proc/self/task").expect("task list");
    i64::try_from(tasks.count()).expect("thread count")
}

/// The threads of this process, as the OS counts them.
#[cfg(not(target_os = "linux"))]
fn process_threads() -> i64 {
    let ps = std::process::Command::new("ps")
        .args(["-M", &std::process::id().to_string()])
        .output()
        .expect("ps");
    let lines = String::from_utf8_lossy(&ps.stdout).lines().count();
    i64::try_from(lines).expect("thread count") - 1
}

/// The threads a load on engine `name` adds to the process. Counts are
/// for the whole process, so other tests must not run at the same time
/// (`--test-threads=1`).
fn new_threads(name: &str, load: impl FnOnce(&mut Engine) -> cactus::Result<Threads>) -> i64 {
    let mut engine = Engine::lock(name).expect("engine");
    let before = process_threads();
    load(&mut engine).expect("weights load");
    process_threads() - before
}

#[test]
#[ignore = "needs the Mirage and Whistle weights"]
fn each_model_keeps_the_threads_it_was_loaded_with() {
    let (vision, speech) = (weights(), read("CACTUS_WHISTLE_WEIGHTS"));
    // A model at n threads adds n - 1 workers, whatever the CPU count.
    let two = new_threads("threads-2", |engine| engine.load_vision(&vision, 2));
    // 13 is above every engine default, so the reset below is visible.
    let many = new_threads("threads-13", |engine| engine.load_vision(&vision, 13));
    assert_eq!(many - two, 11, "{two} and {many} new threads");
    // 0 after an explicit count is the engine's choice again, and the
    // second model on an engine gets its own threads.
    let default = new_threads("threads-0", |engine| engine.load_speech(&speech, 0));
    let after_many = new_threads("threads-13", |engine| engine.load_speech(&speech, 0));
    assert_eq!(after_many, default, "speech at 0 after vision at 13");
    let mut engine = Engine::lock("threads-2").expect("engine");
    assert_eq!(engine.load_vision(&vision, 0), Ok(Threads::Applied(2)));
    assert_eq!(engine.load_vision(&vision, 2), Ok(Threads::Applied(2)));
    assert_eq!(
        engine.load_vision(&vision, 3),
        Ok(Threads::Kept {
            requested: 3,
            current: 2
        })
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

#[test]
#[ignore = "needs the Mirage weights"]
fn named_engines_hold_their_own_models_and_run_at_once() {
    let fresh = Engine::lock("fresh").expect("engine");
    assert_eq!(fresh.models(), cactus::Models::default());
    drop(fresh);
    let pixels = image();
    let describe = |name: &'static str| {
        let pixels = pixels.clone();
        std::thread::spawn(move || {
            let mut engine = loaded_on(name);
            let frame = PackedRgb::new(&pixels, 160, 120).expect("image");
            let started = Instant::now();
            for _ in 0..5 {
                engine
                    .describe(frame, Request::default())
                    .expect("describe");
            }
            started.elapsed()
        })
    };
    let alone = describe("first").join().expect("first");
    let (first, second) = (describe("first"), describe("second"));
    let together = [first.join().expect("first"), second.join().expect("second")];
    eprintln!("5 describes: {alone:?} alone, {together:?} on two engines at once");
    assert!(
        Engine::lock("fresh")
            .expect("engine")
            .models()
            .eq(&cactus::Models::default()),
        "loading on other engines leaves this one empty"
    );
}
