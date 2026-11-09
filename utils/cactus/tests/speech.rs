//! Real-engine speech tests, off the default gate. They need the pinned
//! Whistle weights and the Apollo 13 sample: `mise run cactus:smoke`, or set
//! `CACTUS_WHISTLE_WEIGHTS` and `CACTUS_SPEECH_SAMPLE` and run
//! `cargo test -p cactus --test speech -- --ignored --test-threads=1`.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a missing file or a failing engine call fails the test"
)]

use std::time::Instant;

use cactus::{Engine, Error, SpeechRequest};
use serde_json::Value;

fn read(variable: &str) -> Vec<u8> {
    let path = std::env::var(variable).unwrap_or_else(|_err| panic!("{variable} names a file"));
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading {path}: {err}"))
}

fn weights() -> Vec<u8> {
    read("CACTUS_WHISTLE_WEIGHTS")
}

/// The sample's 16 kHz mono 16-bit samples, from its `data` chunk.
fn sample() -> Vec<f32> {
    let wav = read("CACTUS_SPEECH_SAMPLE");
    let mut rest = wav.get(12..).expect("RIFF header");
    while let Some((header, body)) = rest.split_first_chunk::<8>() {
        let (id, size) = header.split_at(4);
        let size =
            usize::try_from(u32::from_le_bytes(size.try_into().expect("size"))).expect("size");
        let data = body.get(..size).expect("chunk");
        if id == b"data" {
            return data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32768.0)
                .collect();
        }
        rest = body.get(size + size % 2..).unwrap_or_default();
    }
    panic!("no data chunk");
}

fn loaded() -> Engine {
    let mut engine = Engine::lock();
    engine.load_speech(&weights()).expect("Whistle loads");
    engine
}

#[test]
#[ignore = "needs the Whistle weights"]
fn transcribes_with_word_times() {
    let mut engine = loaded();
    assert!(engine.models().speech());
    let pcm = sample();
    let started = Instant::now();
    let transcription = engine
        .transcribe(
            &pcm,
            SpeechRequest {
                word_timestamps: true,
                ..SpeechRequest::default()
            },
        )
        .expect("transcribe");
    let value: Value = serde_json::from_str(&transcription.json).expect("JSON");
    eprintln!(
        "transcribe: {:?}, {} tokens: {}",
        started.elapsed(),
        transcription.tokens,
        value["text"]
    );
    assert_eq!(value["language"], "en");
    assert!(
        value["text"]
            .as_str()
            .is_some_and(|text| text.contains("Houston")),
        "{value}"
    );
    let words = value["words"].as_array().expect("words");
    let duration = f64::from(u32::try_from(pcm.len()).expect("length")) / 16_000.0;
    let mut previous = 0.0;
    for word in words {
        let (start, end) = (
            word["start"].as_f64().expect("start"),
            word["end"].as_f64().expect("end"),
        );
        assert!(
            previous <= start && start <= end && end <= duration,
            "{word}"
        );
        previous = start;
    }
}

#[test]
#[ignore = "needs the Whistle weights"]
fn silence_is_empty_and_short_calls_are_stateless() {
    let mut engine = loaded();
    let silence = engine
        .transcribe(&vec![0.0; 32_000], SpeechRequest::default())
        .expect("silence");
    let value: Value = serde_json::from_str(&silence.json).expect("JSON");
    assert_eq!(
        (&value["text"], &value["language"]),
        (&"".into(), &"".into())
    );
    let pcm = sample();
    let window = pcm.get(32_000..112_000).expect("five seconds");
    let first = engine
        .transcribe(window, SpeechRequest::default())
        .expect("first");
    let second = engine
        .transcribe(window, SpeechRequest::default())
        .expect("second");
    let text = |json: &str| serde_json::from_str::<Value>(json).expect("JSON")["text"].clone();
    assert_eq!(text(&first.json), text(&second.json));
}

#[test]
#[ignore = "needs the Whistle weights"]
fn reports_an_unknown_language_and_refuses_other_weights() {
    let mut engine = loaded();
    let error = engine
        .transcribe(
            &[0.0; 1600],
            SpeechRequest {
                language: Some("en-US"),
                ..SpeechRequest::default()
            },
        )
        .expect_err("unknown language");
    assert!(error.to_string().contains("unknown language"), "{error}");
    let mut other = weights();
    if let Some(last) = other.last_mut() {
        *last ^= 0xff;
    }
    let error = engine.load_speech(&other).expect_err("other weights");
    assert!(matches!(error, Error::ModelInUse { .. }), "{error}");
}

#[test]
#[ignore = "needs the Whistle weights"]
fn grows_a_small_output_buffer() {
    let mut engine = loaded();
    engine.set_output_capacity_for_tests(32);
    let transcription = engine
        .transcribe(
            &sample(),
            SpeechRequest {
                word_timestamps: true,
                ..SpeechRequest::default()
            },
        )
        .expect("transcribe after growing");
    assert!(transcription.grew_to.is_some(), "{transcription:?}");
}

#[test]
#[ignore = "needs the Whistle weights"]
fn streams_in_steps_and_flushes_the_tail_on_stop() {
    let mut engine = loaded();
    let id = engine.open_stream().expect("stream");
    assert!(matches!(engine.open_stream(), Err(Error::StreamInUse)));
    let pcm = sample();
    let mut words = Vec::new();
    let mut received = 0.0_f64;
    for step in pcm.chunks(16_000) {
        let json = engine
            .stream_process(id, step, SpeechRequest::default())
            .expect("step")
            .json;
        let value: Value = serde_json::from_str(&json).expect("JSON");
        received = value["received"].as_f64().expect("received");
        words.extend(value["words"].as_array().expect("words").iter().cloned());
    }
    // A one-off transcription between steps leaves the stream alone.
    let _transcription = engine
        .transcribe(&[0.0; 1600], SpeechRequest::default())
        .expect("transcribe between steps");
    let tail: Value =
        serde_json::from_str(&engine.stream_stop(id).expect("stop").json).expect("JSON");
    words.extend(tail["words"].as_array().expect("words").iter().cloned());
    assert_eq!(tail["pending"], "");
    assert!((received - 30.0).abs() < 0.01, "{received}");
    let text: Vec<&str> = words
        .iter()
        .filter_map(|word| word["word"].as_str())
        .collect();
    eprintln!("streamed: {}", text.join(" "));
    assert!(text.len() > 40, "{text:?}");
    let starts: Vec<f64> = words
        .iter()
        .filter_map(|word| word["start"].as_f64())
        .collect();
    assert!(starts.is_sorted(), "{starts:?}");
    // The next step starts a new stream at time zero.
    let json = engine
        .stream_process(id, &vec![0.0; 16_000], SpeechRequest::default())
        .expect("new stream")
        .json;
    let value: Value = serde_json::from_str(&json).expect("JSON");
    assert_eq!(value["received"].as_f64(), Some(1.0));
    engine.close_stream(id);
    engine.close_stream(id);
    // Closing discarded the audio: a new stream starts at time zero.
    let id = engine.open_stream().expect("released");
    let json = engine
        .stream_process(id, &vec![0.0; 16_000], SpeechRequest::default())
        .expect("reopened stream")
        .json;
    let value: Value = serde_json::from_str(&json).expect("JSON");
    assert_eq!(value["received"].as_f64(), Some(1.0));
    engine.close_stream(id);
}

#[test]
#[ignore = "needs the Whistle weights"]
fn stream_calls_size_the_buffer_instead_of_feeding_audio_twice() {
    let mut engine = loaded();
    let id = engine.open_stream().expect("stream");
    let pcm = sample();
    for step in pcm.get(..80_000).expect("five seconds").chunks(16_000) {
        engine.set_output_capacity_for_tests(32);
        let json = engine
            .stream_process(id, step, SpeechRequest::default())
            .expect("step")
            .json;
        let value: Value = serde_json::from_str(&json).expect("JSON");
        eprintln!("{value}");
    }
    engine.set_output_capacity_for_tests(32);
    let tail: Value =
        serde_json::from_str(&engine.stream_stop(id).expect("stop").json).expect("JSON");
    assert!(
        (tail["received"].as_f64().expect("received") - 5.0).abs() < 0.01,
        "{tail}"
    );
    engine.close_stream(id);
}
