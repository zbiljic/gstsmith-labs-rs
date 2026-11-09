//! `cactusmirage` registration, and opt-in tests with the real engine:
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
    let element = gst::ElementFactory::make("cactusmirage").build()?;
    let factory = element.factory().context("factory")?;
    ensure!(factory.metadata("klass") == Some("Filter/Analyzer/Video"));
    ensure!(factory.rank() == gst::Rank::NONE);
    ensure!(element.property::<Option<String>>("model-file").is_none());
    ensure!(element.property::<u64>("latency") == 0);
    ensure!(element.property::<u64>("analysis-interval") == 0);
    ensure!(element.property::<u32>("max-frame-bytes") == 128 * 1024 * 1024);
    ensure!(element.property::<u32>("threads") == 0);
    ensure!(element.property::<Option<String>>("engine").is_none());
    ensure!(element.property::<bool>("nms"));
    let nicks = |name: &str| -> Result<String> {
        let value = element.property_value(name);
        let class = gst::glib::FlagsClass::with_type(value.type_()).context("flags class")?;
        let bits = value
            .transform::<u32>()
            .context("flag bits")?
            .get::<u32>()?;
        Ok(class.to_nick_string(bits))
    };
    ensure!(nicks("sections")? == "scene|objects|text|relations|reasoning");
    ensure!(nicks("fields")? == "bbox|score|mask|description");
    let sink = factory
        .static_pad_templates()
        .into_iter()
        .find(|template| template.name_template() == "sink")
        .context("sink template")?
        .caps();
    ensure!(
        sink.to_string().contains("format=(string)RGB")
            && sink
                .to_string()
                .contains("pixel-aspect-ratio=(fraction)1/1"),
        "{sink}"
    );
    Ok(())
}

/// Runs `cactusmirage` with the pinned weights on one frame of a test pattern.
fn describe_pattern(width: u32, height: u32, properties: &str) -> Result<gst::Buffer> {
    init()?;
    let weights = std::env::var("CACTUS_MIRAGE_WEIGHTS").context("CACTUS_MIRAGE_WEIGHTS")?;
    let mut harness = gst_check::Harness::new_parse(&format!(
        "videotestsrc num-buffers=1 pattern=smpte ! video/x-raw,format=RGB,width={width},height={height},pixel-aspect-ratio=1/1 \
         ! cactusmirage model-file={weights} {properties}"
    ));
    harness.play();
    let started = std::time::Instant::now();
    let buffer = harness
        .pull()
        .map_err(|err| anyhow::anyhow!("pull: {err:?}"))?;
    eprintln!("{width}x{height} {properties}: {:?}", started.elapsed());
    Ok(buffer)
}

fn document_bytes(buffer: &gst::Buffer) -> Result<(usize, usize)> {
    let meta = buffer
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .context("relation meta")?;
    let docs: Vec<_> = analytics_meta::json::documents(&meta).collect();
    Ok((
        docs.len(),
        docs.iter()
            .map(|doc| doc.schema.len() + doc.json.len())
            .sum(),
    ))
}

#[test]
#[ignore = "needs the Mirage weights"]
fn describes_a_frame_with_the_real_engine() -> Result<()> {
    let buffer = describe_pattern(640, 360, "")?;
    let (count, bytes) = document_bytes(&buffer)?;
    eprintln!("defaults: {count} documents, {bytes} bytes per frame");
    let meta = buffer
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .context("relation meta")?;
    let record = analytics_meta::json::documents_with_schema(&meta, "cactus/mirage/describe")
        .next()
        .context("frame record")?;
    let record: serde_json::Value = serde_json::from_str(record.json)?;
    ensure!(record["engine"] == "4.1.0", "{record}");
    ensure!(record["response"].is_object(), "{record}");
    Ok(())
}

#[test]
#[ignore = "needs the Mirage weights"]
fn measures_output_size_per_frame() -> Result<()> {
    for (width, height, properties) in [
        (640, 360, "json-output=none"),
        (640, 360, "json-output=frame"),
        (640, 360, "json-output=full"),
        (1920, 1080, "json-output=full"),
        (640, 360, "sections=objects fields=bbox+score"),
    ] {
        let buffer = describe_pattern(width, height, properties)?;
        let (count, bytes) = document_bytes(&buffer)?;
        eprintln!("  {count} documents, {bytes} bytes");
    }
    Ok(())
}

#[test]
#[ignore = "needs the Mirage weights"]
fn latest_mode_passes_every_frame() -> Result<()> {
    init()?;
    let weights = std::env::var("CACTUS_MIRAGE_WEIGHTS").context("CACTUS_MIRAGE_WEIGHTS")?;
    // A live source on the system clock, so frames arrive at 30 fps while
    // the worker analyzes the newest one.
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc is-live=true num-buffers=45 pattern=ball \
         ! video/x-raw,format=RGB,width=640,height=360,framerate=30/1,pixel-aspect-ratio=1/1 \
         ! cactusmirage model-file={weights} mode=latest ! appsink name=sink sync=false"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_err| anyhow::anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("sink")
        .context("appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_err| anyhow::anyhow!("not an appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let started = std::time::Instant::now();
    let (mut frames, mut results) = (0, 0);
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
        frames += 1;
        if let Some(meta) = sample
            .buffer()
            .and_then(gst::BufferRef::meta::<gst_analytics::AnalyticsRelationMeta>)
        {
            ensure!(
                analytics_meta::schema::provenance(&meta).is_some(),
                "{}",
                analytics_meta::record::relation_meta(&meta)
            );
            results += 1;
        }
    }
    let elapsed = started.elapsed();
    let stop = std::time::Instant::now();
    pipeline.set_state(gst::State::Null)?;
    eprintln!(
        "latest: {frames} frames in {elapsed:?}, {results} carried a result; stop took {:?}",
        stop.elapsed()
    );
    ensure!(frames == 45 && results > 0 && results < frames);
    Ok(())
}
