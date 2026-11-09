use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};

use super::*;

/// Words the fake engine hears, on the stream timeline in milliseconds.
const SCRIPT: [(&str, i64, i64); 5] = [
    ("Hello", 500, 900),
    ("there.", 900, 1500),
    ("General", 3000, 3500),
    ("Kenobi.", 3500, 4200),
    ("Bye.", 7000, 7600),
];
const AUDIO_MS: i64 = 9000;
/// The PTS of the first sample.
const START: gst::ClockTime = gst::ClockTime::from_seconds(10);
/// Samples carry their own index, so the fake knows where a window starts.
const INDEX_SCALE: f32 = 16_777_216.0;

#[derive(Default)]
struct Calls {
    /// (first ms, length ms) per stateless call.
    windows: Vec<(i64, i64)>,
    steps: usize,
    resets: usize,
    /// The stream reports nothing pending, as the real engine sometimes does
    /// while a word it will confirm later has already started.
    quiet_pending: bool,
    /// Words the windows hear instead of `SCRIPT`.
    script: Option<&'static [(&'static str, i64, i64)]>,
}

type Shared = Arc<Mutex<Calls>>;

#[derive(Default)]
struct Fake {
    calls: Shared,
    /// Stream audio received so far, and the words already committed.
    received_ms: i64,
    committed: usize,
}

fn words_json(words: &[(&str, i64, i64)], origin_ms: i64) -> String {
    let words: Vec<String> = words
        .iter()
        .map(|(text, start, end)| {
            format!(
                r#"{{"word":"{text}","start":{:.2},"end":{:.2},"probability":0.9}}"#,
                seconds(start - origin_ms),
                seconds(end - origin_ms)
            )
        })
        .collect();
    format!("[{}]", words.join(","))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test samples encode non-negative indices below 2^24 exactly"
)]
fn first_ms(pcm: &[f32]) -> i64 {
    // Zero-filled stretches carry no index; any other sample tells where the
    // request starts.
    pcm.iter()
        .position(|sample| *sample != 0.0)
        .and_then(|at| {
            let index = (pcm.get(at)? * INDEX_SCALE).round() as usize;
            Some(sample_ms(index.saturating_sub(at)))
        })
        .unwrap_or(0)
}

impl Fake {
    fn step(&mut self, last: bool) -> String {
        let ready = |end: i64| last || end <= self.received_ms - 1000;
        let fresh: Vec<_> = SCRIPT
            .iter()
            .skip(self.committed)
            .take_while(|(_, _, end)| ready(*end))
            .copied()
            .collect();
        self.committed += fresh.len();
        let quiet = lock(&self.calls).quiet_pending;
        let pending: Vec<_> = SCRIPT
            .iter()
            .skip(self.committed)
            .filter(|(_, start, _)| !quiet && *start < self.received_ms)
            .map(|(text, _, _)| *text)
            .collect();
        format!(
            r#"{{"text":"","words":{},"pending":"{}","language":"en","received":{:.2},"pass_ms":1.0}}"#,
            words_json(&fresh, 0),
            pending.join(" "),
            seconds(self.received_ms)
        )
    }
}

impl Transcriber for Fake {
    fn start(
        &mut self,
        weights: &[u8],
        _threads: u32,
    ) -> Result<(String, cactus::Threads), String> {
        if weights.is_empty() {
            return Err("empty weights".to_owned());
        }
        Ok(("fake-sha".to_owned(), cactus::Threads::EngineDefault))
    }

    fn transcribe(&mut self, pcm: &[f32], _request: &Request) -> Result<String, String> {
        let first = first_ms(pcm);
        let end = first + sample_ms(pcm.len());
        let script = {
            let mut calls = lock(&self.calls);
            calls.windows.push((first, end - first));
            if calls.windows.len() > 100 {
                return Err("the same window again and again".to_owned());
            }
            calls.script.unwrap_or(&SCRIPT)
        };
        // Words that started in the window, over audio, cut off at its end.
        let heard: Vec<_> = script
            .iter()
            .filter(|(_, start, _)| {
                (first..end).contains(start)
                    && pcm
                        .get(ms_sample(start - first))
                        .is_some_and(|sample| *sample != 0.0)
            })
            .map(|(text, start, stop)| (*text, *start, (*stop).min(end)))
            .collect();
        Ok(format!(
            r#"{{"text":"","language":"en","words":{}}}"#,
            words_json(&heard, first)
        ))
    }

    fn open_stream(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn stream(
        &mut self,
        pcm: &[f32],
        _request: &Request,
        last: bool,
    ) -> Result<Vec<String>, String> {
        lock(&self.calls).steps += 1;
        let mut replies = Vec::new();
        if !pcm.is_empty() {
            self.received_ms += sample_ms(pcm.len());
            replies.push(self.step(false));
        }
        if last {
            replies.push(self.step(true));
            self.received_ms = 0;
            self.committed = 0;
        }
        Ok(replies)
    }

    fn reset_stream(&mut self) {
        lock(&self.calls).resets += 1;
        self.received_ms = 0;
        self.committed = 0;
    }
}

/// A tiny temporary file, so tests need no extra crate.
struct TempPath(std::path::PathBuf);

impl TempPath {
    fn weights() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cactuswhistle-{}-{}.cact",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"weights")?;
        Ok(Self(path))
    }

    fn as_str(&self) -> &str {
        self.0.to_str().unwrap_or_default()
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_file(&self.0);
    }
}

fn element(
    properties: &[(&str, glib::Value)],
) -> Result<(super::super::CactusWhistle, Shared, TempPath)> {
    gst::init()?;
    let element: super::super::CactusWhistle = glib::Object::new();
    let calls = Shared::default();
    let shared = Arc::clone(&calls);
    *lock(&element.imp().fixture) = Some(Arc::new(move || {
        Box::new(Fake {
            calls: Arc::clone(&shared),
            ..Fake::default()
        }) as Box<dyn Transcriber>
    }));
    let weights = TempPath::weights()?;
    element.set_property("model-file", weights.as_str());
    for (name, value) in properties {
        element.set_property_from_value(name, value);
    }
    Ok((element, calls, weights))
}

fn whistle(properties: &[(&str, glib::Value)]) -> Result<(gst_check::Harness, Shared, TempPath)> {
    let (element, calls, weights) = element(properties)?;
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.play();
    harness.set_src_caps_str("audio/x-raw,format=F32LE,layout=interleaved,rate=16000,channels=1");
    Ok((harness, calls, weights))
}

/// `ms` of audio from `from_ms`, in 100 ms buffers.
#[expect(
    clippy::cast_precision_loss,
    reason = "test sample indices stay below 2^24"
)]
fn push_audio(harness: &mut gst_check::Harness, from_ms: i64, ms: i64) -> Result<()> {
    for at in (from_ms..from_ms + ms).step_by(100) {
        let first = ms_sample(at);
        let bytes: Vec<u8> = (first..first + ms_sample(100))
            .flat_map(|index| (index as f32 / INDEX_SCALE).to_le_bytes())
            .collect();
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        {
            let buffer = buffer.make_mut();
            buffer.set_pts(START + gst::ClockTime::from_mseconds(u64::try_from(at)?));
            buffer.set_duration(gst::ClockTime::from_mseconds(100));
        }
        harness
            .push(buffer)
            .map_err(|err| anyhow::anyhow!("push at {at} ms: {err:?}"))?;
    }
    Ok(())
}

/// The cues as (PTS ms after `START`, duration ms, text).
fn cues(harness: &mut gst_check::Harness) -> Result<Vec<(u64, u64, String)>> {
    let mut cues = Vec::new();
    while let Some(buffer) = harness.try_pull() {
        let pts = buffer.pts().context("pts")?;
        let map = buffer.map_readable()?;
        cues.push((
            (pts - START).mseconds(),
            buffer.duration().context("duration")?.mseconds(),
            std::str::from_utf8(&map)?.to_owned(),
        ));
    }
    Ok(cues)
}

fn expected() -> Vec<(u64, u64, String)> {
    vec![
        (500, 1000, "Hello there.".to_owned()),
        (3000, 1200, "General Kenobi.".to_owned()),
        (7000, 600, "Bye.".to_owned()),
    ]
}

fn gaps(harness: &mut gst_check::Harness) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        if let gst::EventView::Gap(gap) = event.view() {
            let (pts, duration) = gap.get();
            gaps.push((
                (pts - START).mseconds(),
                duration.map_or(0, gst::ClockTime::mseconds),
            ));
        }
    }
    gaps
}

#[test]
fn windows_commit_each_word_once_on_the_input_timeline() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[])?;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    ensure!(cues(&mut harness)? == expected());
    let windows: Vec<i64> = lock(&calls)
        .windows
        .iter()
        .map(|(first, ms)| first + ms)
        .collect();
    // A window runs once 2 s of audio past the commit point has arrived,
    // which trails the window before by the holdback (1 s) or a held word;
    // EOS transcribes again, for the words the last window held back.
    ensure!(
        windows == [2000, 2900, 3900, 4900, 5500, 6500, 7500, 8500, 9000, 9000],
        "{windows:?}"
    );
    // Text is announced from the first sample, then gaps cover silence.
    let gaps = gaps(&mut harness);
    ensure!(gaps.first() == Some(&(0, 0)), "{gaps:?}");
    ensure!(gaps.iter().any(|(at, _)| *at == 1500), "{gaps:?}");
    Ok(())
}

#[test]
fn a_word_longer_than_a_chunk_waits_for_more_audio() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[])?;
    // Each window cuts the long word at its end, past the holdback, so it
    // commits nothing until the word is over.
    lock(&calls).script = Some(&[
        ("Hello", 500, 900),
        ("Mississippi", 3000, 6000),
        ("Bye.", 7000, 7600),
    ]);
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    let cues = cues(&mut harness)?;
    let words: Vec<&str> = cues
        .iter()
        .flat_map(|(_, _, text)| text.split_whitespace())
        .collect();
    ensure!(words == ["Hello", "Mississippi", "Bye."], "{cues:?}");
    let windows: Vec<i64> = lock(&calls)
        .windows
        .iter()
        .map(|(first, ms)| first + ms)
        .collect();
    // The long word holds the commit point at 3000 from the window ending
    // at 4000 until the one ending at 7000; each one between waits for a
    // second of new audio instead of running again at once.
    ensure!(
        windows == [2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 9000],
        "{windows:?}"
    );
    Ok(())
}

#[test]
fn an_upstream_gap_is_silence_on_the_timeline() -> Result<()> {
    let (mut harness, _calls, _weights) = whistle(&[])?;
    push_audio(&mut harness, 0, 2000)?;
    ensure!(
        harness.push_event(
            gst::event::Gap::builder(START + gst::ClockTime::from_seconds(2))
                .duration(gst::ClockTime::from_seconds(3))
                .build()
        )
    );
    push_audio(&mut harness, 5000, 4000)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    let cues = cues(&mut harness)?;
    ensure!(
        cues == [
            (500, 1000, "Hello there.".to_owned()),
            (7000, 600, "Bye.".to_owned())
        ],
        "{cues:?}"
    );
    Ok(())
}

#[test]
fn tags_the_detected_language_once() -> Result<()> {
    let (mut harness, _calls, _weights) = whistle(&[])?;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    let mut languages = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        if let gst::EventView::Tag(tag) = event.view()
            && let Some(language) = tag.tag().get::<gst::tags::LanguageCode>()
        {
            languages.push(language.get().to_owned());
        }
    }
    ensure!(languages == ["en"], "{languages:?}");
    Ok(())
}

#[test]
fn the_whole_recording_at_eos() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[("chunk-duration", 0u32.to_value())])?;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(cues(&mut harness)?.is_empty());
    ensure!(harness.push_event(gst::event::Eos::new()));
    ensure!(cues(&mut harness)? == expected());
    ensure!(lock(&calls).windows == [(0, AUDIO_MS)]);
    Ok(())
}

#[test]
fn stream_mode_groups_the_engine_words_and_flushes_at_eos() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[
        ("mode", Mode::Stream.to_value()),
        ("chunk-duration", 1000u32.to_value()),
    ])?;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    ensure!(cues(&mut harness)? == expected());
    ensure!(lock(&calls).steps == 10, "nine steps and the stop");
    Ok(())
}

#[test]
fn stream_words_confirmed_after_a_quiet_step_are_kept() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[
        ("mode", Mode::Stream.to_value()),
        ("chunk-duration", 1000u32.to_value()),
    ])?;
    lock(&calls).quiet_pending = true;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    ensure!(cues(&mut harness)? == expected());
    Ok(())
}

#[test]
fn a_flush_restarts_the_stream() -> Result<()> {
    let (mut harness, calls, _weights) = whistle(&[
        ("mode", Mode::Stream.to_value()),
        ("chunk-duration", 1000u32.to_value()),
    ])?;
    push_audio(&mut harness, 0, 3000)?;
    let resets = lock(&calls).resets;
    ensure!(harness.push_event(gst::event::FlushStart::new()));
    ensure!(harness.push_event(gst::event::FlushStop::new(true)));
    ensure!(lock(&calls).resets > resets);
    ensure!(
        harness.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    let _before = cues(&mut harness)?;
    push_audio(&mut harness, 0, AUDIO_MS)?;
    ensure!(harness.push_event(gst::event::Eos::new()));
    ensure!(cues(&mut harness)? == expected());
    Ok(())
}

#[test]
fn reports_latency_from_the_chunks() -> Result<()> {
    let (mut harness, _calls, _weights) = whistle(&[("agreement", 2000u32.to_value())])?;
    harness.set_upstream_latency(gst::ClockTime::from_mseconds(20));
    let latency = harness.query_latency().context("latency")?;
    // An open cue (8 s), a chunk and the agreement, then processing.
    ensure!(
        latency == gst::ClockTime::from_mseconds(20 + 8000 + 2000 + 2000 + 2000),
        "{latency}"
    );
    Ok(())
}

#[test]
fn refuses_bad_settings_at_start() -> Result<()> {
    gst::init()?;
    for properties in [
        vec![("language", "en-US".to_value())],
        vec![("holdback", 2000u32.to_value())],
        vec![
            ("mode", Mode::Stream.to_value()),
            ("chunk-duration", 0u32.to_value()),
        ],
    ] {
        let (element, _calls, _weights) = element(&properties)?;
        ensure!(
            element.set_state(gst::State::Paused).is_err(),
            "{properties:?}"
        );
        element.set_state(gst::State::Null)?;
    }
    let (element, _calls, _weights) = element(&[])?;
    element.set_property("model-file", None::<&str>);
    ensure!(
        element.set_state(gst::State::Paused).is_err(),
        "no model-file"
    );
    element.set_state(gst::State::Null)?;
    Ok(())
}
