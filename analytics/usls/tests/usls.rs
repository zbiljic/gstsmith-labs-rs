use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gst_analytics::prelude::*;

fn initialize() -> Result<()> {
    static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| {
            gst::init().map_err(|e| e.to_string())?;
            gstusls::plugin_register_static().map_err(|e| e.to_string())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

#[test]
fn factory_registers_without_assets_and_requires_a_model() -> Result<()> {
    initialize()?;
    let element = gst::ElementFactory::make("uslsobjectdetect").build()?;
    ensure!(
        (element.property::<f32>("score-threshold") - 0.5).abs() < f32::EPSILON,
        "public default"
    );
    let _failure = element
        .set_state(gst::State::Paused)
        .expect_err("model-file required");
    element.set_state(gst::State::Null)?;
    Ok(())
}

#[cfg(not(all(feature = "coreml", target_os = "macos", target_arch = "aarch64")))]
#[test]
fn unavailable_coreml_fails_explicitly() -> Result<()> {
    initialize()?;
    let pipeline = gst::Pipeline::new();
    let element = gst::ElementFactory::make("uslsobjectdetect").build()?;
    pipeline.add(&element)?;
    element.set_property("model-file", "missing.onnx");
    element.set_property_from_str("execution-provider", "coreml");
    let _failure = element
        .set_state(gst::State::Paused)
        .expect_err("unavailable CoreML");
    let message = pipeline
        .bus()
        .context("bus")?
        .timed_pop_filtered(gst::ClockTime::from_seconds(1), &[gst::MessageType::Error])
        .context("provider-specific error message")?;
    let gst::MessageView::Error(error) = message.view() else {
        anyhow::bail!("expected error");
    };
    ensure!(
        error
            .debug()
            .is_some_and(|debug| debug.contains("CoreML is unavailable")),
        "provider-specific rejection: {error:?}"
    );
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

#[cfg(all(feature = "coreml", target_os = "macos", target_arch = "aarch64"))]
#[test]
fn new_profiles_reject_unqualified_coreml_units_before_model_load() -> Result<()> {
    initialize()?;
    for profile in [
        "rtdetr-v1-r18",
        "rtdetr-v2-s",
        "rtdetr-v4-s",
        "picodet-l-coco",
        "dfine-n-coco",
        "deim-dfine-s-coco",
        "deimv2-atto-coco",
    ] {
        for units in ["all", "cpu-and-neural-engine"] {
            let pipeline = gst::Pipeline::new();
            let element = gst::ElementFactory::make("uslsobjectdetect").build()?;
            pipeline.add(&element)?;
            element.set_property("model-file", "missing.onnx");
            element.set_property_from_str("model-profile", profile);
            element.set_property_from_str("execution-provider", "coreml");
            element.set_property_from_str("coreml-compute-units", units);
            let _failure = element
                .set_state(gst::State::Paused)
                .expect_err("unqualified combination");
            let message = pipeline
                .bus()
                .context("bus")?
                .timed_pop_filtered(gst::ClockTime::from_seconds(1), &[gst::MessageType::Error])
                .context("provider-specific rejection")?;
            let gst::MessageView::Error(error) = message.view() else {
                anyhow::bail!("expected error");
            };
            ensure!(
                error.debug().is_some_and(
                    |debug| debug.contains("unqualified") && debug.contains("cpu-and-gpu")
                ),
                "explicit rejection of {profile}/{units}: {error:?}"
            );
            pipeline.set_state(gst::State::Null)?;
        }
    }
    Ok(())
}

fn verify_detections(buffer: &gst::BufferRef, width: u32, height: u32) -> Result<()> {
    let meta = buffer
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .context("detection relation metadata")?;
    let objects = meta
        .iter::<gst_analytics::AnalyticsODMtd>()
        .collect::<Vec<_>>();
    ensure!(
        objects
            .iter()
            .any(|od| od.obj_type() == Some(gst::glib::Quark::from_str("person"))),
        "sample must yield a person"
    );
    for object in objects {
        let location = object.location()?;
        ensure!(
            location.loc_conf_lvl.is_finite()
                && location.x >= 0
                && location.y >= 0
                && location.w > 0
                && location.h > 0
                && i64::from(location.x) + i64::from(location.w) <= i64::from(width)
                && i64::from(location.y) + i64::from(location.h) <= i64::from(height),
            "finite source-frame location"
        );
        ensure!(
            meta.iter::<gst_analytics::AnalyticsClassificationMtd>()
                .any(|class| meta.relation(object.id(), class.id())
                    == gst_analytics::RelTypes::RELATE_TO
                    && class
                        .iterate()
                        .any(|(label, score)| Some(label) == object.obj_type()
                            && (score - object.confidence_level()).abs() < f32::EPSILON)),
            "matching related classification"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires exact local qualified graph and person JPEG sample; no downloads"]
fn real_model() -> Result<()> {
    initialize()?;
    let model = PathBuf::from(
        std::env::var("GSTSMITH_USLS_MODEL").context("GSTSMITH_USLS_MODEL is required")?,
    );
    let sample = PathBuf::from(
        std::env::var("GSTSMITH_USLS_SAMPLE").context("GSTSMITH_USLS_SAMPLE is required")?,
    );
    ensure!(
        model.is_file() && sample.is_file(),
        "local model and sample files required"
    );
    let provider = std::env::var("GSTSMITH_USLS_PROVIDER").unwrap_or_else(|_| "cpu".into());
    ensure!(
        provider == "cpu" || provider == "coreml",
        "invalid test provider"
    );
    let profile = std::env::var("GSTSMITH_USLS_PROFILE").unwrap_or_else(|_| "rfdetr-nano".into());
    let image = usls::Image::try_read(&sample)?;
    let info =
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, image.width(), image.height())
            .build()?;
    let row = usize::try_from(image.width())?
        .checked_mul(3)
        .context("row overflow")?;
    let stride = usize::try_from(*info.stride().first().context("stride")?)?;
    let mut pixels = vec![0_u8; info.size()];
    for (source, target) in image
        .as_raw()
        .chunks_exact(row)
        .zip(pixels.chunks_mut(stride))
    {
        target
            .get_mut(..row)
            .context("row destination")?
            .copy_from_slice(source);
    }
    // Two independent elements each restart. Every initialization verifies its own
    // CoreML assignment rather than reusing evidence from the preceding session.
    for _ in 0..2 {
        let element = gst::ElementFactory::make("uslsobjectdetect").build()?;
        element.set_property("model-file", model.to_str().context("UTF-8 model path")?);
        element.set_property_from_str("model-profile", &profile);
        element.set_property("score-threshold", 0.55_f32);
        element.set_property_from_str("execution-provider", &provider);
        for _ in 0..2 {
            let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
            harness.set_src_caps(info.to_caps()?);
            harness.play();
            let mut input = gst::Buffer::from_mut_slice(pixels.clone());
            input.make_mut().set_pts(gst::ClockTime::from_seconds(1));
            input
                .make_mut()
                .set_duration(gst::ClockTime::from_mseconds(40));
            let output = harness.push_and_pull(input)?;
            ensure!(
                output.size() == info.size() && output.map_readable()?.as_slice() == pixels,
                "original frame payload"
            );
            ensure!(output.pts() == Some(gst::ClockTime::from_seconds(1)), "PTS");
            ensure!(
                output.duration() == Some(gst::ClockTime::from_mseconds(40)),
                "duration"
            );
            verify_detections(&output, image.width(), image.height())?;
            ensure!(harness.push_event(gst::event::Eos::new()), "EOS accepted");
        }
    }

    overlay_pipeline(
        &model,
        &sample,
        &profile,
        &provider,
        image.width(),
        image.height(),
    )?;
    println!(
        "real-model {profile}/{provider}: person/source coordinates/relations/pixels/PTS/restarts/overlay/EOS passed"
    );
    Ok(())
}

fn overlay_pipeline(
    model: &std::path::Path,
    sample: &std::path::Path,
    profile: &str,
    provider: &str,
    width: u32,
    height: u32,
) -> Result<()> {
    // Inspect metadata before a standard overlay consumes it, then require finite EOS.
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("filesrc")
        .property("location", sample.to_str().context("UTF-8 sample path")?)
        .build()?;
    let decoder = gst::ElementFactory::make("jpegdec").build()?;
    let converter = gst::ElementFactory::make("videoconvert").build()?;
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        )
        .build()?;
    let detector = gst::ElementFactory::make("uslsobjectdetect")
        .property("model-file", model.to_str().context("model path")?)
        .property("score-threshold", 0.55_f32)
        .build()?;
    detector.set_property_from_str("model-profile", profile);
    detector.set_property_from_str("execution-provider", provider);
    let overlay = gst::ElementFactory::make("objectdetectionoverlay").build()?;
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()?;
    pipeline.add_many([
        &source, &decoder, &converter, &filter, &detector, &overlay, &sink,
    ])?;
    gst::Element::link_many([
        &source, &decoder, &converter, &filter, &detector, &overlay, &sink,
    ])?;
    let inspected = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&inspected);
    detector.static_pad("src").context("src pad")?.add_probe(
        gst::PadProbeType::BUFFER,
        move |_pad, probe| {
            if let Some(buffer) = probe.buffer()
                && let Ok(mut result) = capture.lock()
            {
                *result = Some(verify_detections(buffer, width, height).map_err(|e| e.to_string()));
            }
            gst::PadProbeReturn::Ok
        },
    );
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().context("bus")?;
    let message = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;
    let message = message.context("finite pipeline timed out")?;
    ensure!(
        message.type_() == gst::MessageType::Eos,
        "pipeline failed: {message:?}"
    );
    inspected
        .lock()
        .map_err(|_error| anyhow::anyhow!("inspection lock"))?
        .take()
        .context("no pre-overlay frame")?
        .map_err(anyhow::Error::msg)?;
    println!(
        "real-model {profile}/{provider}: person/source coordinates/relations/pixels/PTS/restarts/overlay/EOS passed"
    );
    Ok(())
}
