//! `cactuswhistle` registration, and opt-in tests with the real engine:
//! `mise run //analytics/cactus:smoke`.

use anyhow::{Context, Result, ensure};
use gst::prelude::*;

fn init() -> Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    let mut result = Ok(());
    INIT.call_once(|| {
        result = gst::init().and_then(|()| {
            gstcactus::plugin_register_static()
                .map_err(|err| gst::glib::Error::new(gst::CoreError::Failed, &err.to_string()))
        });
    });
    Ok(result?)
}

#[test]
fn registers_with_documented_defaults() -> Result<()> {
    init()?;
    let element = gst::ElementFactory::make("cactuswhistle").build()?;
    let factory = element.factory().context("factory")?;
    ensure!(factory.metadata("klass") == Some("Text/Audio/Filter"));
    ensure!(factory.metadata("long-name") == Some("Transcriber"));
    ensure!(element.property::<Option<String>>("model-file").is_none());
    ensure!(
        element
            .property_value("mode")
            .get::<&gst::glib::EnumValue>()?
            .nick()
            == "windows"
    );
    ensure!(element.property::<u32>("chunk-duration") == 2000);
    ensure!(element.property::<u32>("context-duration") == 10_000);
    ensure!(element.property::<u32>("holdback") == 1000);
    ensure!(element.property::<u32>("agreement") == 0);
    ensure!(element.property::<u32>("latency") == 0);
    ensure!(element.property::<Option<String>>("engine").is_none());
    let src = factory
        .static_pad_templates()
        .into_iter()
        .find(|template| template.name_template() == "src")
        .context("src template")?
        .caps();
    ensure!(
        src.to_string() == "text/x-raw, format=(string)utf8",
        "{src}"
    );
    Ok(())
}

fn path(variable: &str) -> Result<String> {
    std::env::var(variable).with_context(|| format!("{variable} is not set"))
}

/// Transcribes the Apollo 13 sample and returns (PTS ms, duration ms, text)
/// per cue.
fn transcribe(properties: &str) -> Result<Vec<(u64, u64, String)>> {
    init()?;
    let pipeline = gst::parse::launch(&format!(
        "filesrc location={} ! wavparse ! audioconvert ! audioresample ! \
         audio/x-raw,format=F32LE,rate=16000,channels=1 ! \
         cactuswhistle model-file={} {properties} ! appsink name=sink sync=false",
        path("CACTUS_SPEECH_SAMPLE")?,
        path("CACTUS_WHISTLE_WEIGHTS")?
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_pipeline| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .context("sink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_element| anyhow::anyhow!("not an appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let mut cues = Vec::new();
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(60)) {
        let buffer = sample.buffer().context("buffer")?;
        let map = buffer.map_readable()?;
        cues.push((
            buffer.pts().context("pts")?.mseconds(),
            buffer.duration().context("duration")?.mseconds(),
            std::str::from_utf8(&map)?.to_owned(),
        ));
    }
    let bus = pipeline.bus().context("bus")?;
    let error = bus.pop_filtered(&[gst::MessageType::Error]);
    pipeline.set_state(gst::State::Null)?;
    if let Some(message) = error
        && let gst::MessageView::Error(error) = message.view()
    {
        anyhow::bail!("{} ({:?})", error.error(), error.debug());
    }
    for (pts, duration, text) in &cues {
        eprintln!("{pts:>6} +{duration:>5} {text}");
    }
    Ok(cues)
}

fn check(cues: &[(u64, u64, String)]) -> Result<()> {
    ensure!(cues.len() >= 5, "{cues:?}");
    ensure!(
        cues.iter().any(|(_, _, text)| text.contains("problem")),
        "{cues:?}"
    );
    ensure!(
        cues.windows(2)
            .all(|pair| matches!(pair, [a, b] if a.0 + a.1 <= b.0)),
        "cues are in order and do not overlap: {cues:?}"
    );
    ensure!(
        cues.iter()
            .all(|(pts, duration, _)| pts + duration <= 30_000),
        "{cues:?}"
    );
    Ok(())
}

#[test]
#[ignore = "needs the Whistle weights and the speech sample"]
fn transcribes_in_rolling_windows() -> Result<()> {
    check(&transcribe("")?)
}

fn word_count(cues: &[(u64, u64, String)]) -> usize {
    cues.iter()
        .map(|(_, _, text)| text.split_whitespace().count())
        .sum()
}

#[test]
#[ignore = "needs the Whistle weights and the speech sample"]
fn transcribes_with_the_engine_stream() -> Result<()> {
    let streamed = transcribe("mode=stream chunk-duration=1000")?;
    check(&streamed)?;
    // Words the engine confirms late are kept: the stream says about as
    // much as the whole recording.
    let whole = transcribe("chunk-duration=0")?;
    let (streamed, whole) = (word_count(&streamed), word_count(&whole));
    ensure!(5 * streamed >= 4 * whole, "{streamed} of {whole} words");
    Ok(())
}

#[test]
#[ignore = "needs the Whistle weights and the speech sample"]
fn transcribes_the_whole_recording() -> Result<()> {
    check(&transcribe("chunk-duration=0")?)
}

#[test]
#[ignore = "needs the Whistle weights and the speech sample"]
fn streams_on_two_engines_at_once() -> Result<()> {
    let first = std::thread::spawn(|| transcribe("mode=stream chunk-duration=1000 engine=one"));
    let second = transcribe("mode=stream chunk-duration=1000 engine=two");
    let first = first
        .join()
        .map_err(|_panic| anyhow::anyhow!("panicked"))??;
    check(&first)?;
    check(&second?)
}

#[test]
#[ignore = "needs the Whistle weights and the speech sample"]
fn one_engine_has_one_stream() -> Result<()> {
    init()?;
    let pipeline = gst::parse::launch(&format!(
        "audiotestsrc ! audio/x-raw,format=F32LE,rate=16000,channels=1 ! tee name=t \
         t. ! queue ! cactuswhistle model-file={weights} mode=stream engine=shared ! fakesink \
         t. ! queue ! cactuswhistle model-file={weights} mode=stream engine=shared ! fakesink",
        weights = path("CACTUS_WHISTLE_WEIGHTS")?
    ))?;
    let started = pipeline.set_state(gst::State::Paused);
    let bus = pipeline.bus().context("bus")?;
    let error = bus.pop_filtered(&[gst::MessageType::Error]);
    pipeline.set_state(gst::State::Null)?;
    ensure!(started.is_err(), "the second stream is refused");
    let message = error.context("an error message")?;
    let gst::MessageView::Error(error) = message.view() else {
        anyhow::bail!("not an error: {message:?}");
    };
    let text = format!("{} {}", error.error(), error.debug().unwrap_or_default());
    ensure!(
        text.contains("Cactus engine \"shared\" holds one speech stream"),
        "{text}"
    );
    Ok(())
}

#[test]
#[ignore = "needs the Whistle and Mirage weights and the speech sample"]
fn runs_beside_cactusmirage() -> Result<()> {
    init()?;
    let mirage = gst::parse::launch(&format!(
        "videotestsrc num-buffers=10 ! video/x-raw,width=320,height=240 ! videoconvert ! \
         cactusmirage model-file={} engine=vision ! fakesink",
        path("CACTUS_MIRAGE_WEIGHTS")?
    ))?;
    mirage.set_state(gst::State::Playing)?;
    // On the default engine, beside Mirage on its own.
    let speech = std::thread::spawn(|| transcribe("mode=stream chunk-duration=1000"));
    let bus = mirage.bus().context("bus")?;
    let done = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(120),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .context("mirage finishes")?;
    mirage.set_state(gst::State::Null)?;
    ensure!(matches!(done.view(), gst::MessageView::Eos(_)), "{done:?}");
    check(
        &speech
            .join()
            .map_err(|_panic| anyhow::anyhow!("panicked"))??,
    )
}
