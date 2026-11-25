//! `diarizer` registration, and opt-in tests with the real speaker model on
//! labelled conversations: `mise run //analytics/diarization:smoke`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use analytics_meta::schema;
use anyhow::{Context as _, Result, ensure};
use gst::prelude::*;

fn init() -> Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    let mut result = Ok(());
    INIT.call_once(|| {
        result = gst::init().and_then(|()| {
            gstdiarization::plugin_register_static()
                .map_err(|err| gst::glib::Error::new(gst::CoreError::Failed, &err.to_string()))
        });
    });
    Ok(result?)
}

#[test]
fn registers_with_documented_defaults() -> Result<()> {
    init()?;
    let element = gst::ElementFactory::make("diarizer").build()?;
    let factory = element.factory().context("factory")?;
    ensure!(factory.metadata("klass") == Some("Text/Audio/Filter"));
    ensure!(element.property::<Option<String>>("model-file").is_none());
    let nick = |name| -> Result<String> {
        Ok(element
            .property_value(name)
            .get::<&gst::glib::EnumValue>()?
            .nick()
            .to_owned())
    };
    ensure!(nick("model-profile")? == "auto");
    ensure!(nick("label-mode")? == "on-change");
    ensure!(element.property::<u32>("threads") == 0);
    ensure!(element.property::<f64>("match-threshold").abs() < f64::EPSILON);
    ensure!(element.property::<f64>("short-match-threshold").abs() < f64::EPSILON);
    ensure!(element.property::<f64>("halves-threshold").abs() < f64::EPSILON);
    ensure!(element.property::<u32>("min-new-duration") == 1500);
    ensure!(element.property::<u32>("min-update-duration") == 1000);
    ensure!(element.property::<u32>("max-speakers") == 0);
    ensure!(element.property::<u32>("audio-buffer-duration") == 60_000);
    ensure!(element.property::<String>("label-format") == "{label}: ");
    ensure!(element.property::<String>("unknown-label").is_empty());
    ensure!(
        element
            .property::<gst::Structure>("speaker-names")
            .n_fields()
            == 0
    );
    let names: Vec<_> = factory
        .static_pad_templates()
        .into_iter()
        .map(|template| template.name_template().to_string())
        .collect();
    ensure!(names == ["audio_sink", "text_sink", "src"], "{names:?}");
    ensure!(element.property::<Option<String>>("model-config").is_none());
    // No model: the element refuses to start.
    ensure!(element.set_state(gst::State::Paused).is_err());
    element.set_state(gst::State::Null)?;
    Ok(())
}

/// The detail of the error `diarizer` posts when it fails to start with
/// `properties`.
fn start_error(properties: &[(&str, &str)]) -> Result<String> {
    init()?;
    let pipeline = gst::Pipeline::new();
    let element = gst::ElementFactory::make("diarizer")
        .property("model-file", "no-such-model.onnx")
        .build()?;
    for (name, value) in properties {
        element.set_property_from_str(name, value);
    }
    pipeline.add(&element)?;
    ensure!(pipeline.set_state(gst::State::Paused).is_err(), "started");
    // Read before stopping: going to NULL flushes the bus.
    let bus = pipeline.bus().context("bus")?;
    let message = bus.pop_filtered(&[gst::MessageType::Error]);
    pipeline.set_state(gst::State::Null)?;
    let message = message.context("no error message")?;
    let gst::MessageView::Error(error) = message.view() else {
        anyhow::bail!("not an error");
    };
    Ok(error
        .debug()
        .map(|debug| debug.to_string())
        .unwrap_or_default())
}

#[test]
fn model_config_is_only_read_by_the_custom_profile() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("diarizer-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "name = \"x\"\n")?;
    let broken = broken.to_string_lossy().into_owned();
    let missing = dir.join("missing.toml").to_string_lossy().into_owned();
    let results = [
        start_error(&[("model-profile", "custom")]),
        start_error(&[("model-profile", "custom"), ("model-config", "")]),
        start_error(&[("model-config", &broken)]),
        start_error(&[("model-profile", "custom"), ("model-config", &broken)]),
        start_error(&[("model-profile", "custom"), ("model-config", &missing)]),
    ];
    std::fs::remove_dir_all(&dir)?;
    let [no_config, empty_config, not_custom, unusable, unreadable] = results;
    for no_config in [no_config?, empty_config?] {
        ensure!(no_config.contains("needs model-config"), "{no_config}");
    }
    let not_custom = not_custom?;
    ensure!(not_custom.contains("only read with"), "{not_custom}");
    let unusable = unusable?;
    ensure!(
        unusable.contains("broken.toml")
            && unusable.contains("missing field `input`")
            && !unusable.contains("line 1"),
        "{unusable}"
    );
    let unreadable = unreadable?;
    ensure!(
        unreadable.contains("reading") && unreadable.contains("missing.toml"),
        "{unreadable}"
    );
    Ok(())
}

/// Every sample `sink` gives until EOS. Fails on a pipeline error, or when
/// nothing arrives for a minute, instead of waiting for an EOS that never
/// comes.
fn pull_all(pipeline: &gst::Pipeline, sink: &gst_app::AppSink) -> Result<Vec<gst::Sample>> {
    let bus = pipeline.bus().context("bus")?;
    let mut samples = Vec::new();
    let mut idle = std::time::Duration::ZERO;
    let step = std::time::Duration::from_millis(100);
    loop {
        if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
            anyhow::bail!("pipeline error: {message:?}");
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            samples.push(sample);
            idle = std::time::Duration::ZERO;
        } else if sink.is_eos() {
            return Ok(samples);
        } else {
            idle += step;
            ensure!(
                idle < std::time::Duration::from_secs(60),
                "the pipeline stalled"
            );
        }
    }
}

fn path(variable: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .with_context(|| format!("{variable} is not set"))
}

/// Decodes an audio file to 16 kHz mono F32.
fn decode(file: &Path) -> Result<Vec<f32>> {
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! decodebin ! audioconvert ! audioresample \
         ! audio/x-raw,format=F32LE,rate=16000,channels=1 ! appsink name=out sync=false",
        file.display()
    ))?
    .downcast::<gst::Pipeline>()
    .ok()
    .context("not a pipeline")?;
    let sink = pipeline
        .by_name("out")
        .context("appsink")?
        .downcast::<gst_app::AppSink>()
        .ok()
        .context("not an appsink")?;
    pipeline.set_state(gst::State::Playing)?;
    let pulled = pull_all(&pipeline, &sink);
    pipeline.set_state(gst::State::Null)?;
    let mut samples = Vec::new();
    for sample in pulled? {
        let buffer = sample.buffer().context("buffer")?;
        let map = buffer.map_readable()?;
        let (chunks, _) = map.as_slice().as_chunks::<4>();
        samples.extend(chunks.iter().map(|b| f32::from_le_bytes(*b)));
    }
    Ok(samples)
}

/// A labelled turn, in seconds.
#[derive(Clone, Debug)]
struct Turn {
    start: f64,
    end: f64,
    speaker: String,
}

/// The turns of `file` in an RTTM file.
fn rttm(rttm: &Path, file: &str) -> Result<Vec<Turn>> {
    std::fs::read_to_string(rttm)?
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.get(1) == Some(&file)).then_some(fields)
        })
        .map(|fields| {
            let start: f64 = fields.get(3).context("onset")?.parse()?;
            let duration: f64 = fields.get(4).context("duration")?.parse()?;
            Ok(Turn {
                start,
                end: start + duration,
                speaker: (*fields.get(7).context("speaker")?).to_owned(),
            })
        })
        .collect()
}

/// The stretches where one speaker talks alone, cut into cues of at most
/// 6 s the way a transcriber groups words. Stretches under 0.3 s are left
/// out, as a transcriber rarely makes a cue of them.
fn cues(turns: &[Turn]) -> Vec<Turn> {
    let mut edges: Vec<f64> = turns.iter().flat_map(|t| [t.start, t.end]).collect();
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    let mut alone: Vec<Turn> = Vec::new();
    for pair in edges.windows(2) {
        let [start, end] = *pair else {
            continue;
        };
        let middle = f64::midpoint(start, end);
        let mut talking = turns.iter().filter(|t| t.start <= middle && middle < t.end);
        let (Some(turn), None) = (talking.next(), talking.next()) else {
            continue;
        };
        match alone.last_mut() {
            Some(last) if last.speaker == turn.speaker && (last.end - start).abs() < 1e-6 => {
                last.end = end;
            }
            _ => alone.push(Turn {
                start,
                end,
                speaker: turn.speaker.clone(),
            }),
        }
    }
    let mut cues = Vec::new();
    for stretch in alone.into_iter().filter(|t| t.end - t.start >= 0.3) {
        let parts = ((stretch.end - stretch.start) / 6.0).ceil().max(1.0);
        let step = (stretch.end - stretch.start) / parts;
        let mut start = stretch.start;
        while start < stretch.end - 1e-6 {
            cues.push(Turn {
                start,
                end: (start + step).min(stretch.end),
                speaker: stretch.speaker.clone(),
            });
            start += step;
        }
    }
    cues
}

/// A labelled conversation: audio and its cues.
struct Conversation {
    name: String,
    samples: Vec<f32>,
    cues: Vec<Turn>,
}

fn samples_dir() -> Result<PathBuf> {
    path("DIARIZATION_SAMPLES")
}

fn conversation(wav: &str, rttm_file: &str, file: &str) -> Result<Conversation> {
    let dir = samples_dir()?;
    Ok(Conversation {
        name: file.to_owned(),
        samples: decode(&dir.join(wav))?,
        cues: cues(&rttm(&dir.join(rttm_file), file)?),
    })
}

/// Speakers from different recordings taking turns: each piece is a cue of
/// one conversation, played in round-robin order with a short pause.
fn mixed(parts: &[Conversation]) -> Conversation {
    let gap = vec![0.0; 4800];
    let mut samples = Vec::new();
    let mut cues = Vec::new();
    let longest = parts.iter().map(|c| c.cues.len()).max().unwrap_or(0);
    for index in 0..longest {
        for part in parts {
            let Some(cue) = part.cues.get(index) else {
                continue;
            };
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "sample positions"
            )]
            let range = (cue.start * 16_000.0) as usize..(cue.end * 16_000.0) as usize;
            let Some(audio) = part.samples.get(range) else {
                continue;
            };
            samples.extend_from_slice(&gap);
            #[expect(clippy::cast_precision_loss, reason = "a time")]
            let start = samples.len() as f64 / 16_000.0;
            samples.extend_from_slice(audio);
            #[expect(clippy::cast_precision_loss, reason = "a time")]
            let end = samples.len() as f64 / 16_000.0;
            cues.push(Turn {
                start,
                end,
                speaker: format!("{}/{}", part.name, cue.speaker),
            });
        }
    }
    Conversation {
        name: "mixed".to_owned(),
        samples,
        cues,
    }
}

fn clock(seconds: f64) -> gst::ClockTime {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test times"
    )]
    let ns = (seconds * 1e9).round() as u64;
    gst::ClockTime::from_nseconds(ns)
}

/// Runs the conversation through `diarizer` and returns each cue's speaker.
fn diarize(
    conversation: &Conversation,
    model: &Path,
    properties: &[(&str, &str)],
) -> Result<Vec<schema::Speaker>> {
    init()?;
    let pipeline = gst::Pipeline::new();
    let audio = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 16_000_i32)
                .field("channels", 1_i32)
                .build(),
        )
        .format(gst::Format::Time)
        .build();
    let text = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build(),
        )
        .format(gst::Format::Time)
        .build();
    let diarizer = gst::ElementFactory::make("diarizer")
        .property("model-file", model.to_string_lossy().as_ref())
        .property("threads", 4_u32)
        .property("audio-buffer-duration", 3_600_000_u32)
        .build()?;
    for (name, value) in properties {
        diarizer.set_property_from_str(name, value);
    }
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline.add_many([
        audio.upcast_ref(),
        text.upcast_ref(),
        &diarizer,
        sink.upcast_ref(),
    ])?;
    audio.link_pads(None, &diarizer, Some("audio_sink"))?;
    text.link_pads(None, &diarizer, Some("text_sink"))?;
    diarizer.link_pads(Some("src"), &sink, None)?;
    pipeline.set_state(gst::State::Playing)?;

    for (index, chunk) in conversation.samples.chunks(1600).enumerate() {
        let bytes: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut buffer = gst::Buffer::from_slice(bytes);
        let buffer_ref = buffer.get_mut().context("writable")?;
        buffer_ref.set_pts(gst::ClockTime::from_mseconds(u64::try_from(index)? * 100));
        audio.push_buffer(buffer)?;
    }
    audio.end_of_stream()?;
    for (index, cue) in conversation.cues.iter().enumerate() {
        let mut buffer = gst::Buffer::from_slice(format!("cue {index}").into_bytes());
        let buffer_ref = buffer.get_mut().context("writable")?;
        buffer_ref.set_pts(clock(cue.start));
        buffer_ref.set_duration(clock(cue.end).saturating_sub(clock(cue.start)));
        text.push_buffer(buffer)?;
    }
    text.end_of_stream()?;

    let pulled = pull_all(&pipeline, &sink);
    pipeline.set_state(gst::State::Null)?;
    let mut speakers = Vec::new();
    for sample in pulled? {
        let buffer = sample.buffer().context("buffer")?;
        let meta = buffer
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .context("relation meta")?;
        speakers.push(schema::speaker(&meta).context("x-speaker")?);
    }
    ensure!(
        speakers.len() == conversation.cues.len(),
        "a cue went missing"
    );
    Ok(speakers)
}

/// Cue accuracy by duration, with each found speaker counted as the true
/// speaker it shares most audio with, and how many speakers were found.
fn score(cues: &[Turn], found: &[schema::Speaker]) -> (f64, usize, usize) {
    let mut overlap: HashMap<(u32, &str), f64> = HashMap::new();
    for (cue, speaker) in cues.iter().zip(found) {
        if let Some(n) = speaker.n {
            *overlap.entry((n, cue.speaker.as_str())).or_default() += cue.end - cue.start;
        }
    }
    let mut best: HashMap<u32, (&str, f64)> = HashMap::new();
    for (&(n, truth), &seconds) in &overlap {
        let entry = best.entry(n).or_insert((truth, 0.0));
        if seconds > entry.1 {
            *entry = (truth, seconds);
        }
    }
    let total: f64 = cues.iter().map(|c| c.end - c.start).sum();
    let right: f64 = cues
        .iter()
        .zip(found)
        .filter(|(cue, speaker)| {
            speaker
                .n
                .and_then(|n| best.get(&n))
                .is_some_and(|(truth, _)| *truth == cue.speaker)
        })
        .map(|(cue, _)| cue.end - cue.start)
        .sum();
    let truth: std::collections::HashSet<_> = cues.iter().map(|c| c.speaker.as_str()).collect();
    (right / total, best.len(), truth.len())
}

fn check(conversation: &Conversation, min_accuracy: f64, extra_speakers: usize) -> Result<()> {
    let model = path("DIARIZATION_WESPEAKER_MODEL")?;
    let found = diarize(conversation, &model, &[])?;
    let (accuracy, speakers, truth) = score(&conversation.cues, &found);
    let unknown = found.iter().filter(|s| s.n.is_none()).count();
    for (cue, speaker) in conversation.cues.iter().zip(&found) {
        println!(
            "  {:6.2}-{:6.2} {:>14} -> {} {}",
            cue.start,
            cue.end,
            cue.speaker,
            speaker.label.as_deref().unwrap_or("?"),
            speaker
                .confidence
                .map_or_else(String::new, |c| format!("{c:.2}"))
        );
    }
    println!(
        "{}: {} cues, accuracy {:.1}%, {speakers} speakers for {truth}, {unknown} unknown",
        conversation.name,
        conversation.cues.len(),
        accuracy * 100.0
    );
    ensure!(accuracy >= min_accuracy, "accuracy {accuracy:.3}");
    ensure!(
        (truth..=truth + extra_speakers).contains(&speakers),
        "{speakers} speakers for {truth}"
    );
    Ok(())
}

#[test]
#[ignore = "needs the speaker model and samples: mise run //analytics/diarization:smoke"]
fn two_speakers_talking() -> Result<()> {
    init()?;
    check(
        &conversation("sample.wav", "sample.rttm", "sample")?,
        // Measured 91%; collapsing to one speaker gives 46%.
        0.85,
        0,
    )
}

#[test]
#[ignore = "needs the speaker model and samples: mise run //analytics/diarization:smoke"]
fn speakers_of_several_recordings() -> Result<()> {
    init()?;
    let parts = [
        conversation("sample.wav", "sample.rttm", "sample")?,
        conversation("dev00.wav", "debug.development.rttm", "dev00")?,
    ];
    check(&mixed(&parts), 0.9, 1)
}

/// The example `custom` profile in the README: the built-in
/// `wespeaker-resnet34` profile written out.
fn readme_config() -> Result<String> {
    let readme = include_str!("../README.md");
    let (_, rest) = readme.split_once("```toml\n").context("a toml block")?;
    let (config, _) = rest.split_once("```").context("its end")?;
    Ok(config.to_owned())
}

#[test]
#[ignore = "needs the speaker model and samples: mise run //analytics/diarization:smoke"]
fn a_custom_profile_runs_the_model_like_the_builtin_one() -> Result<()> {
    init()?;
    let model = path("DIARIZATION_WESPEAKER_MODEL")?;
    let config = std::env::temp_dir().join(format!("diarizer-custom-{}.toml", std::process::id()));
    std::fs::write(&config, readme_config()?)?;
    let conversation = conversation("sample.wav", "sample.rttm", "sample")?;
    let builtin = diarize(&conversation, &model, &[]);
    let custom = diarize(
        &conversation,
        &model,
        &[
            ("model-profile", "custom"),
            ("model-config", &config.to_string_lossy()),
        ],
    );
    std::fs::remove_file(&config)?;
    let (builtin, custom) = (builtin?, custom?);
    for (one, other) in builtin.iter().zip(&custom) {
        ensure!(one.model == "wespeaker-resnet34" && other.model == "wespeaker-copy");
        ensure!(
            one.n == other.n && one.confidence == other.confidence,
            "{one:?} against {other:?}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "needs the speaker model and samples: mise run //analytics/diarization:smoke"]
fn meeting_excerpts() -> Result<()> {
    init()?;
    for file in ["dev00", "dev01"] {
        let conversation = conversation(&format!("{file}.wav"), "debug.development.rttm", file)?;
        check(&conversation, 0.9, 1)?;
    }
    Ok(())
}
