use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, ensure};
use gst_check::Harness;

use super::*;
use crate::embedder::Error;
use crate::profile::{self, Profile};

/// Hears a speaker in the level of the audio: 0.1 and 0.2 are two voices.
struct Fake {
    profile: Profile,
}

impl SegmentEmbedder for Fake {
    fn profile(&self) -> &Profile {
        &self.profile
    }

    fn embed(&mut self, pcm: &[f32]) -> Result<Vec<f32>, Error> {
        let min = self.profile.min_samples;
        if pcm.len() < min {
            return Err(Error::TooShort {
                samples: pcm.len(),
                min,
            });
        }
        #[expect(clippy::cast_precision_loss, reason = "a test level")]
        let level = pcm.iter().map(|v| v.abs()).sum::<f32>() / pcm.len() as f32;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small positive level"
        )]
        let axis = (level * 10.0).round() as usize % 8;
        let mut embedding = vec![0.0; 8];
        if let Some(value) = embedding.get_mut(axis) {
            *value = 1.0;
        }
        Ok(embedding)
    }
}

const RATE: u64 = 16_000;
const A: f32 = 0.1;
const B: f32 = 0.2;

struct Fixture {
    element: super::super::Diarizer,
    audio: Harness,
    text: Harness,
    bus: gst::Bus,
}

fn diarizer(properties: &[(&str, glib::Value)]) -> Result<Fixture> {
    gst::init()?;
    let element: super::super::Diarizer = glib::Object::new();
    for (name, value) in properties {
        element.set_property_from_value(name, value);
    }
    *lock(&element.imp().fixture) = Some(Arc::new(|| {
        Box::new(Fake {
            profile: profile::wespeaker_resnet34(),
        })
    }));
    let bus = gst::Bus::new();
    element.set_bus(Some(&bus));
    let mut audio = Harness::with_element(&element, Some("audio_sink"), None);
    let mut text = Harness::with_element(&element, Some("text_sink"), Some("src"));
    text.play();
    audio.set_src_caps_str("audio/x-raw,format=F32LE,layout=interleaved,rate=16000,channels=1");
    text.set_src_caps_str("text/x-raw,format=utf8");
    Ok(Fixture {
        element,
        audio,
        text,
        bus,
    })
}

/// Pushes `level` from `from_ms` to `to_ms` in 100 ms buffers.
fn push_audio(audio: &mut Harness, from_ms: u64, to_ms: u64, level: f32) -> Result<()> {
    let mut ms = from_ms;
    while ms < to_ms {
        let len = (to_ms - ms).min(100);
        let samples = usize::try_from(len * RATE / 1000)?;
        let bytes: Vec<u8> = std::iter::repeat_n(level.to_le_bytes(), samples)
            .flatten()
            .collect();
        let mut buffer = gst::Buffer::from_slice(bytes);
        {
            let buffer = buffer.get_mut().context("writable")?;
            buffer.set_pts(gst::ClockTime::from_mseconds(ms));
            buffer.set_duration(gst::ClockTime::from_mseconds(len));
        }
        audio.push(buffer)?;
        ms += len;
    }
    Ok(())
}

/// The voices A, B, A, then digital silence, over 10 s.
fn conversation(audio: &mut Harness) -> Result<()> {
    push_audio(audio, 0, 3000, A)?;
    push_audio(audio, 3000, 6000, B)?;
    push_audio(audio, 6000, 9000, A)?;
    push_audio(audio, 9000, 10_000, 0.0)
}

/// Pushes a cue and returns its output text and speaker document.
fn cue(
    text: &mut Harness,
    from_ms: u64,
    to_ms: u64,
    words: &str,
) -> Result<(String, schema::Speaker)> {
    let mut buffer = gst::Buffer::from_slice(words.as_bytes().to_vec());
    {
        let buffer = buffer.get_mut().context("writable")?;
        buffer.set_pts(gst::ClockTime::from_mseconds(from_ms));
        buffer.set_duration(gst::ClockTime::from_mseconds(to_ms - from_ms));
    }
    text.push(buffer)?;
    let out = text.pull()?;
    ensure!(out.pts() == Some(gst::ClockTime::from_mseconds(from_ms)));
    let meta = out
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .context("relation meta")?;
    let speaker = schema::speaker(&meta).context("x-speaker")?;
    let map = out.map_readable()?;
    Ok((String::from_utf8(map.as_slice().to_vec())?, speaker))
}

fn speaker_changes(text: &mut Harness) -> Vec<String> {
    std::iter::from_fn(|| text.try_pull_event())
        .filter_map(|event| {
            let structure = event.structure()?;
            (structure.name() == SPEAKER_CHANGE)
                .then(|| structure.get::<String>("speaker").ok())
                .flatten()
        })
        .collect()
}

fn messages(bus: &gst::Bus, name: &str) -> Vec<gst::Structure> {
    std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::Element]))
        .filter_map(|message| message.structure().map(ToOwned::to_owned))
        .filter(|structure| structure.name() == name)
        .collect()
}

#[test]
fn cues_get_speakers_documents_events_and_messages() -> Result<()> {
    let mut f = diarizer(&[])?;
    conversation(&mut f.audio)?;
    let cues = [
        (200, 2800, "one"),
        (3200, 5800, "two"),
        (6200, 7000, "three"),
        (7100, 8800, "four"),
        (8850, 8950, "hm"),
        (9100, 9900, "five"),
    ];
    let mut out = Vec::new();
    for (from, to, words) in cues {
        out.push(cue(&mut f.text, from, to, words)?);
    }
    let texts: Vec<_> = out.iter().map(|(text, _)| text.as_str()).collect();
    ensure!(
        texts == ["S1: one", "S2: two", "S1: three", "four", "hm", "five"],
        "{texts:?}"
    );
    let numbers: Vec<_> = out.iter().map(|(_, s)| s.n).collect();
    ensure!(
        numbers == [Some(1), Some(2), Some(1), Some(1), None, None],
        "{numbers:?}"
    );
    let new: Vec<_> = out.iter().map(|(_, s)| s.new).collect();
    ensure!(new == [true, true, false, false, false, false]);
    let (_, first) = out.first().context("first")?;
    ensure!(first.model == "wespeaker-resnet34" && !first.known);
    ensure!(first.label.as_deref() == Some("S1") && first.name.is_none());
    ensure!(first.id.as_ref().is_some_and(|id| id.len() == 8));
    let (_, third) = out.get(2).context("third")?;
    ensure!(third.id == first.id);
    ensure!(third.confidence.is_some_and(|c| c > 0.99));
    let (_, silent) = out.last().context("last")?;
    ensure!(silent.id.is_none() && silent.label.is_none() && silent.confidence.is_none());

    ensure!(speaker_changes(&mut f.text) == ["S1", "S2", "S1"]);
    let new = messages(&f.bus, "diarizer/speaker-new");
    ensure!(new.len() == 2);
    let texts: Vec<_> = new
        .iter()
        .filter_map(|s| s.get::<String>("text").ok())
        .collect();
    ensure!(texts == ["one", "two"], "{texts:?}");
    // The first pop drained the bus, so count per-cue messages again.
    let mut f = diarizer(&[])?;
    conversation(&mut f.audio)?;
    cue(&mut f.text, 200, 2800, "one")?;
    cue(&mut f.text, 9100, 9900, "five")?;
    let speakers = messages(&f.bus, "diarizer/speaker");
    ensure!(speakers.len() == 2);
    let first = speakers.first().context("first message")?;
    ensure!(first.get::<u32>("n")? == 1 && first.get::<String>("label")? == "S1");
    ensure!(first.get::<bool>("new")? && first.get::<String>("text")? == "one");
    let silent = speakers.get(1).context("second message")?;
    ensure!(!silent.has_field("n") && silent.get::<String>("model")? == "wespeaker-resnet34");
    Ok(())
}

#[test]
fn names_label_modes_and_merges() -> Result<()> {
    gst::init()?;
    let names = gst::Structure::builder("names")
        .field("S2", "Haise")
        .build();
    let mut f = diarizer(&[
        ("speaker-names", names.to_value()),
        ("label-mode", LabelMode::Always.to_value()),
        ("label-format", "[{label}] ".to_value()),
        ("unknown-label", "? ".to_value()),
        ("min-new-duration", 1000_u32.to_value()),
    ])?;
    conversation(&mut f.audio)?;
    let (one, first) = cue(&mut f.text, 200, 1500, "one")?;
    ensure!(one == "[S1] one");
    ensure!(cue(&mut f.text, 1600, 2800, "again")?.0 == "[S1] again");
    ensure!(
        f.element
            .emit_by_name::<bool>("set-speaker-name", &[&"S1", &"Lousma"])
    );
    ensure!(
        !f.element
            .emit_by_name::<bool>("set-speaker-name", &[&"nobody", &"X"])
    );
    let (two, document) = cue(&mut f.text, 3200, 4500, "two")?;
    ensure!(two == "[Haise] two");
    ensure!(document.name.as_deref() == Some("Haise"));
    let id2 = document.id.context("id")?;
    ensure!(
        f.element
            .emit_by_name::<bool>("set-speaker-name", &[&id2, &"Fred"])
    );
    let names = f.element.property::<gst::Structure>("speaker-names");
    ensure!(names.get::<String>("S1")? == "Lousma");
    ensure!(names.get::<String>(id2.as_str())? == "Fred");
    ensure!(
        !f.element
            .emit_by_name::<bool>("merge-speakers", &[&"S1", &"S9"])
    );
    ensure!(
        f.element
            .emit_by_name::<bool>("merge-speakers", &[&"S1", &"S2"])
    );
    // B's voice now belongs to S1, whose name wins.
    ensure!(cue(&mut f.text, 4600, 5800, "three")?.0 == "[Lousma] three");
    // Clearing by one key clears every key S1 goes by: S1, S2 and the ids.
    let id1 = first.id.context("id")?;
    ensure!(
        f.element
            .emit_by_name::<bool>("set-speaker-name", &[&id1, &""])
    );
    ensure!(cue(&mut f.text, 6200, 8800, "four")?.0 == "[S1] four");
    ensure!(cue(&mut f.text, 9100, 9900, "five")?.0 == "? five");
    ensure!(speaker_changes(&mut f.text) == ["S1", "Haise"]);
    Ok(())
}

#[test]
fn lost_audio_and_ended_audio_give_unknown_speakers() -> Result<()> {
    let mut f = diarizer(&[
        ("audio-buffer-duration", 1000_u32.to_value()),
        ("min-new-duration", 1000_u32.to_value()),
    ])?;
    push_audio(&mut f.audio, 0, 10_000, A)?;
    let (text, document) = cue(&mut f.text, 200, 2800, "early")?;
    ensure!(text == "early" && document.n.is_none(), "{document:?}");
    let (text, document) = cue(&mut f.text, 9000, 10_000, "late")?;
    ensure!(text == "S1: late" && document.new);
    ensure!(f.audio.push_event(gst::event::Eos::new()));
    let begin = Instant::now();
    let (_, document) = cue(&mut f.text, 12_000, 13_000, "after")?;
    ensure!(document.n.is_none());
    ensure!(
        begin.elapsed() < AUDIO_WAIT / 2,
        "waited for audio after EOS"
    );
    Ok(())
}

#[test]
fn audio_caps_follow_the_profile() -> Result<()> {
    let f = diarizer(&[])?;
    let pad = f.element.static_pad("audio_sink").context("audio_sink")?;
    let caps = pad.query_caps(None);
    ensure!(
        caps.to_string()
            == "audio/x-raw, format=(string)F32LE, layout=(string)interleaved, rate=(int)16000, channels=(int)1",
        "{caps}"
    );
    let wrong = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", 48_000_i32)
        .field("channels", 1_i32)
        .build();
    ensure!(!pad.query_accept_caps(&wrong));
    Ok(())
}

/// A segment from `start`, whose running time starts at 0.
fn segment_from(start: gst::ClockTime) -> gst::Event {
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_start(start);
    segment.set_time(start);
    gst::event::Segment::new(&segment)
}

fn cue_buffer(from_ms: u64, to_ms: u64, words: &str) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_slice(words.as_bytes().to_vec());
    if let Some(buffer) = buffer.get_mut() {
        buffer.set_pts(gst::ClockTime::from_mseconds(from_ms));
        buffer.set_duration(gst::ClockTime::from_mseconds(to_ms - from_ms));
    }
    buffer
}

/// Pushes a cue from another thread, so the test can act while it waits.
fn push_in_background(
    text: &Harness,
    from_ms: u64,
    to_ms: u64,
) -> Result<std::thread::JoinHandle<(Result<gst::FlowSuccess, gst::FlowError>, Duration)>> {
    let pad = text.srcpad().context("text src pad")?;
    let buffer = cue_buffer(from_ms, to_ms, "waiting");
    Ok(std::thread::spawn(move || {
        let begin = Instant::now();
        (pad.push(buffer), begin.elapsed())
    }))
}

fn joined<T>(handle: std::thread::JoinHandle<T>) -> Result<T> {
    handle
        .join()
        .map_err(|_panic| anyhow::anyhow!("the pushing thread panicked"))
}

#[test]
fn a_cue_waits_for_its_audio() -> Result<()> {
    let mut f = diarizer(&[])?;
    let waiting = push_in_background(&f.text, 200, 2800)?;
    std::thread::sleep(Duration::from_millis(100));
    push_audio(&mut f.audio, 0, 3000, A)?;
    let (pushed, elapsed) = joined(waiting)?;
    ensure!(
        pushed.is_ok() && elapsed < AUDIO_WAIT,
        "{pushed:?} after {elapsed:?}"
    );
    let out = f.text.pull()?;
    let map = out.map_readable()?;
    ensure!(map.as_slice() == b"S1: waiting");
    Ok(())
}

#[test]
fn a_cue_without_audio_waits_once() -> Result<()> {
    let mut f = diarizer(&[])?;
    let begin = Instant::now();
    ensure!(cue(&mut f.text, 200, 2800, "one")?.1.n.is_none());
    ensure!(begin.elapsed() >= AUDIO_WAIT);
    let begin = Instant::now();
    ensure!(cue(&mut f.text, 3200, 5800, "two")?.1.n.is_none());
    ensure!(
        begin.elapsed() < AUDIO_WAIT / 2,
        "waited again without audio"
    );
    push_audio(&mut f.audio, 5000, 9000, A)?;
    ensure!(cue(&mut f.text, 6200, 8800, "three")?.0 == "S1: three");
    Ok(())
}

#[test]
fn flushing_or_stopping_releases_a_waiting_cue() -> Result<()> {
    let mut f = diarizer(&[])?;
    let waiting = push_in_background(&f.text, 200, 2800)?;
    std::thread::sleep(Duration::from_millis(100));
    ensure!(f.text.push_event(gst::event::FlushStart::new()));
    let (pushed, elapsed) = joined(waiting)?;
    ensure!(pushed == Err(gst::FlowError::Flushing), "{pushed:?}");
    ensure!(elapsed < AUDIO_WAIT * 4 / 5, "released after {elapsed:?}");
    ensure!(f.text.push_event(gst::event::FlushStop::new(true)));
    ensure!(f.text.push_event(segment_from(gst::ClockTime::ZERO)));

    let waiting = push_in_background(&f.text, 3200, 5800)?;
    std::thread::sleep(Duration::from_millis(100));
    f.element.set_state(gst::State::Ready)?;
    let (pushed, elapsed) = joined(waiting)?;
    ensure!(
        pushed.is_err() && elapsed < AUDIO_WAIT * 4 / 5,
        "{pushed:?} after {elapsed:?}"
    );
    Ok(())
}

#[test]
fn gaps_and_segments_keep_audio_and_cues_on_running_time() -> Result<()> {
    let mut f = diarizer(&[])?;
    // Audio from PTS 10 s and cues from PTS 100 s both start at running
    // time 0.
    ensure!(
        f.audio
            .push_event(segment_from(gst::ClockTime::from_seconds(10)))
    );
    ensure!(
        f.text
            .push_event(segment_from(gst::ClockTime::from_seconds(100)))
    );
    push_audio(&mut f.audio, 10_000, 13_000, A)?;
    ensure!(
        f.audio.push_event(
            gst::event::Gap::builder(gst::ClockTime::from_seconds(13))
                .duration(gst::ClockTime::from_seconds(2))
                .build()
        )
    );
    push_audio(&mut f.audio, 15_000, 18_000, B)?;
    ensure!(cue(&mut f.text, 100_200, 102_800, "one")?.0 == "S1: one");
    let (text, document) = cue(&mut f.text, 103_200, 104_800, "gap")?;
    ensure!(
        text == "gap" && document.n.is_none(),
        "the audio gap is silence"
    );
    ensure!(cue(&mut f.text, 105_200, 107_800, "two")?.0 == "S2: two");
    // A text gap tells that no cue starts before its end, so audio before
    // it goes once more audio arrives.
    ensure!(
        f.text.push_event(
            gst::event::Gap::builder(gst::ClockTime::from_seconds(108))
                .duration(gst::ClockTime::from_seconds(10))
                .build()
        )
    );
    push_audio(&mut f.audio, 18_000, 19_000, B)?;
    ensure!(cue(&mut f.text, 108_200, 108_900, "late")?.1.n.is_none());
    Ok(())
}

#[test]
fn latency_and_rules_follow_the_properties() -> Result<()> {
    let mut f = diarizer(&[])?;
    f.text.set_upstream_latency(gst::ClockTime::from_seconds(2));
    let mut query = gst::query::Latency::new();
    let src = f.element.static_pad("src").context("src")?;
    ensure!(src.query(&mut query));
    let (_, min, _) = query.result();
    ensure!(
        min == gst::ClockTime::from_mseconds(2000 + PROCESSING_MS),
        "{min}"
    );
    let matching = |f: &Fixture| {
        lock(&f.element.imp().state)
            .registry
            .as_ref()
            .map(|registry| registry.rules().matching)
    };
    ensure!(matching(&f) == Some(0.4));

    let mut f = diarizer(&[
        ("match-threshold", 0.7_f64.to_value()),
        ("max-speakers", 1_u32.to_value()),
    ])?;
    ensure!(matching(&f).is_some_and(|m| (m - 0.7).abs() < 1e-6));
    conversation(&mut f.audio)?;
    ensure!(cue(&mut f.text, 200, 2800, "one")?.0 == "S1: one");
    let (two, document) = cue(&mut f.text, 3200, 5800, "two")?;
    ensure!(
        two == "two" && document.n == Some(1),
        "a stranger joins the only speaker"
    );
    Ok(())
}

#[test]
fn every_named_model_profile_value_is_a_builtin_profile() -> Result<()> {
    gst::init()?;
    let class = glib::EnumClass::with_type(ModelProfile::static_type()).context("enum")?;
    let nicks: Vec<_> = class
        .values()
        .iter()
        .map(|value| value.nick().to_owned())
        .filter(|nick| !profile::RESERVED.contains(&nick.as_str()))
        .collect();
    let builtin: Vec<_> = profile::builtin().into_iter().map(|p| p.name).collect();
    ensure!(nicks == builtin, "{nicks:?} against {builtin:?}");
    for value in class.values() {
        let profile = value.to_value(&class).get::<ModelProfile>()?;
        ensure!(
            profile
                .name()
                .is_none_or(|name| profile::by_name(name).is_some())
        );
    }
    Ok(())
}

#[test]
fn audio_that_jumps_back_starts_a_new_timeline() -> Result<()> {
    let mut f = diarizer(&[("audio-buffer-duration", 10_000_u32.to_value())])?;
    push_audio(&mut f.audio, 100_000, 103_000, A)?;
    ensure!(cue(&mut f.text, 100_200, 102_800, "before")?.0 == "S1: before");
    // A restarted source: audio and cues from 10 s again.
    push_audio(&mut f.audio, 10_000, 13_000, B)?;
    let (text, document) = cue(&mut f.text, 10_200, 12_800, "after")?;
    ensure!(text == "S2: after" && document.new, "{text} {document:?}");
    Ok(())
}

#[test]
fn other_rates_are_refused() -> Result<()> {
    let f = diarizer(&[])?;
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_rate(2.0);
    for pad in ["audio_sink", "text_sink"] {
        let pad = f.element.static_pad(pad).context("pad")?;
        ensure!(
            !pad.send_event(gst::event::Segment::new(&segment)),
            "{}",
            pad.name()
        );
    }
    Ok(())
}

#[test]
fn a_shown_unknown_label_is_followed_by_a_label() -> Result<()> {
    let mut f = diarizer(&[("unknown-label", "? ".to_value())])?;
    push_audio(&mut f.audio, 0, 3000, A)?;
    push_audio(&mut f.audio, 3000, 4000, 0.0)?;
    push_audio(&mut f.audio, 4000, 7000, A)?;
    ensure!(cue(&mut f.text, 200, 2800, "one")?.0 == "S1: one");
    ensure!(cue(&mut f.text, 3100, 3900, "silent")?.0 == "? silent");
    // Unlabelled, it would read as the unknown speaker's.
    ensure!(cue(&mut f.text, 4200, 6800, "two")?.0 == "S1: two");
    ensure!(
        speaker_changes(&mut f.text) == ["S1"],
        "no change of speaker"
    );
    Ok(())
}
