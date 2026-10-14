use anyhow::{Context, Result, ensure};
use gst::prelude::*;
use gst_analytics::prelude::*;
use std::sync::OnceLock;

fn initialize() -> Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        gst::init().map_err(|e| e.to_string())?;
        gstusls::plugin_register_static().map_err(|e| e.to_string())
    })
    .clone()
    .map_err(anyhow::Error::msg)
}

#[test]
fn every_profile_registers_without_assets_and_missing_files_fail() -> Result<()> {
    initialize()?;
    for profile in [
        "sam",
        "sam-hq",
        "mobile-sam",
        "edge-sam",
        "sam2",
        "sam2.1",
        "sam3-tracker",
        "fastsam",
        "ecdetseg-coco",
        "yoloe-v8-pf",
        "yoloe-11-pf",
        "yoloe-26-pf",
        "birefnet-cod",
        "birefnet-dis",
        "birefnet-hrsod",
        "birefnet-massive",
    ] {
        let e = gst::ElementFactory::make("uslssegment").build()?;
        e.set_property_from_str("model-profile", profile);
        ensure!(
            e.find_property("model-profile").is_some(),
            "registered profile"
        );
        e.set_state(gst::State::Ready)?;
        let _failure = e
            .set_state(gst::State::Paused)
            .expect_err("missing local models");
        e.set_state(gst::State::Null)?;
    }
    Ok(())
}

#[test]
#[ignore = "requires explicit local segmentation assets in GSTSMITH_SEGMENT_* variables"]
#[expect(
    clippy::too_many_lines,
    reason = "keep the opt-in end-to-end qualification in one test"
)]
fn real_segmentation() -> Result<()> {
    initialize()?;
    let profile = std::env::var("GSTSMITH_SEGMENT_PROFILE").context("GSTSMITH_SEGMENT_PROFILE")?;
    let sample = std::env::var("GSTSMITH_SEGMENT_SAMPLE").context("GSTSMITH_SEGMENT_SAMPLE")?;
    let image = usls::Image::try_read(&sample)?;
    let info =
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, image.width(), image.height())
            .build()?;
    let row = usize::try_from(image.width())?
        .checked_mul(3)
        .context("row")?;
    let stride = usize::try_from(*info.stride().first().context("stride")?)?;
    let mut pixels = vec![0; info.size()];
    for (source, destination) in image
        .as_raw()
        .chunks_exact(row)
        .zip(pixels.chunks_mut(stride))
    {
        destination
            .get_mut(..row)
            .context("row extent")?
            .copy_from_slice(source);
    }
    for _ in 0..2 {
        let e = gst::ElementFactory::make("uslssegment").build()?;
        e.set_property_from_str("model-profile", &profile);
        if let Ok(provider) = std::env::var("GSTSMITH_SEGMENT_PROVIDER") {
            e.set_property_from_str("execution-provider", &provider);
        }
        for (env, property) in [
            ("GSTSMITH_SEGMENT_MODEL", "model-file"),
            ("GSTSMITH_SEGMENT_ENCODER", "encoder-file"),
            ("GSTSMITH_SEGMENT_DECODER", "decoder-file"),
            ("GSTSMITH_SEGMENT_PROMPT", "prompt"),
        ] {
            if let Ok(value) = std::env::var(env) {
                e.set_property(property, value);
            }
        }
        for _ in 0..2 {
            if let Err(error) = e.set_state(gst::State::Paused) {
                e.set_state(gst::State::Null)?;
                anyhow::bail!(
                    "starting {profile}: {error}; use GST_DEBUG=uslssegment:5 for details"
                );
            }
            let mut h = gst_check::Harness::with_element(&e, Some("sink"), Some("src"));
            let caps = info.to_caps()?;
            h.set_src_caps(caps.clone());
            h.play();
            let mut input = gst::Buffer::from_mut_slice(pixels.clone());
            input.make_mut().set_pts(gst::ClockTime::from_seconds(1));
            input
                .make_mut()
                .set_duration(gst::ClockTime::from_mseconds(40));
            // A same-buffer detector box exercises prompt association for SAM profiles.
            let object = {
                let mut meta = gst_analytics::AnalyticsRelationMeta::add(input.make_mut());
                meta.add_od_mtd(
                    gst::glib::Quark::from_str("object"),
                    0,
                    0,
                    i32::try_from(image.width())?,
                    i32::try_from(image.height())?,
                    0.99,
                )?
                .id()
            };
            let output = h.push_and_pull(input)?;
            ensure!(
                output.map_readable()?.as_slice() == pixels
                    && output.pts() == Some(gst::ClockTime::from_seconds(1)),
                "carrier/PTS"
            );
            let meta = output
                .meta::<gst_analytics::AnalyticsRelationMeta>()
                .context("segmentation metadata")?;
            let masks = meta
                .iter::<gst_analytics::AnalyticsAnyMtd>()
                .filter_map(|m| segmentation_meta::mask(&meta, m.id()).map(|mask| (m.id(), mask)))
                .collect::<Vec<_>>();
            ensure!(!masks.is_empty(), "sample must yield segmentation masks");
            let prompted =
                profile.starts_with("sam") || profile == "mobile-sam" || profile == "edge-sam";
            for (id, mask) in &masks {
                let video = mask
                    .meta::<gst_video::VideoMeta>()
                    .context("mask video meta")?;
                ensure!(
                    (video.width(), video.height()) == (image.width(), image.height()),
                    "source-sized mask"
                );
                let data = mask.map_readable()?;
                ensure!(
                    data.as_slice().iter().all(|v| *v <= 1) && data.as_slice().contains(&1),
                    "nonempty binary regions"
                );
                if prompted && std::env::var_os("GSTSMITH_SEGMENT_PROMPT").is_none() {
                    ensure!(
                        meta.relation(object, *id) == gst_analytics::RelTypes::RELATE_TO,
                        "original detection relation"
                    );
                }
            }
            let mut overlay = gst_check::Harness::new("segmentationoverlay");
            overlay.set_src_caps(caps);
            overlay.play();
            overlay.push_and_pull(output)?;
            ensure!(h.push_event(gst::event::Eos::new()), "EOS");
        }
    }
    println!(
        "{profile}: masks, pixels, timestamps, relations, restarts, standard overlay and EOS passed"
    );
    Ok(())
}

fn fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/segment/{name}.onnx",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one table-driven test covers every public model profile"
)]
fn every_family_executes_its_native_contract_and_preserves_prompt_relations() -> Result<()> {
    initialize()?;
    for (profile, model, encoder, decoder) in [
        ("fastsam", Some("fastsam"), None, None),
        ("ecdetseg-coco", Some("ecdetseg"), None, None),
        ("yoloe-v8-pf", Some("yoloe"), None, None),
        ("yoloe-11-pf", Some("yoloe"), None, None),
        ("yoloe-26-pf", Some("yoloe26"), None, None),
        ("birefnet-cod", Some("birefnet"), None, None),
        ("birefnet-dis", Some("birefnet"), None, None),
        ("birefnet-hrsod", Some("birefnet"), None, None),
        ("birefnet-massive", Some("birefnet"), None, None),
        ("sam", None, Some("encoder"), Some("sam-decoder")),
        ("mobile-sam", None, Some("encoder"), Some("sam-decoder")),
        ("sam-hq", None, Some("hq-encoder"), Some("hq-decoder")),
        ("edge-sam", None, Some("encoder"), Some("edge-decoder")),
        (
            "sam2",
            None,
            Some("hierarchical-encoder"),
            Some("sam2-decoder"),
        ),
        (
            "sam2.1",
            None,
            Some("hierarchical-encoder"),
            Some("sam2-decoder"),
        ),
        (
            "sam3-tracker",
            None,
            Some("hierarchical-encoder"),
            Some("sam3-decoder"),
        ),
    ] {
        let e = gst::ElementFactory::make("uslssegment").build()?;
        e.set_property_from_str("model-profile", profile);
        for (name, property) in [
            (model, "model-file"),
            (encoder, "encoder-file"),
            (decoder, "decoder-file"),
        ] {
            if let Some(name) = name {
                e.set_property(property, fixture(name));
            }
        }
        e.set_state(gst::State::Paused)
            .with_context(|| format!("starting {profile}"))?;
        let mut h = gst_check::Harness::with_element(&e, Some("sink"), Some("src"));
        h.set_src_caps_str(
            "video/x-raw,format=RGB,width=4,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
        );
        h.play();
        let mut input = gst::Buffer::from_mut_slice(vec![128_u8; 24]);
        input.make_mut().set_pts(gst::ClockTime::from_seconds(7));
        let ids = {
            let mut meta = gst_analytics::AnalyticsRelationMeta::add(input.make_mut());
            let a = meta
                .add_od_mtd(gst::glib::Quark::from_str("first"), 0, 0, 2, 2, 0.9)?
                .id();
            let b = meta
                .add_od_mtd(gst::glib::Quark::from_str("second"), 2, 0, 2, 2, 0.8)?
                .id();
            [a, b]
        };
        let output = h
            .push_and_pull(input)
            .with_context(|| format!("running {profile}"))?;
        ensure!(
            output.map_readable()?.as_slice() == [128_u8; 24]
                && output.pts() == Some(gst::ClockTime::from_seconds(7)),
            "{profile}: carrier"
        );
        let meta = output
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .context("meta")?;
        let masks = meta
            .iter::<gst_analytics::AnalyticsAnyMtd>()
            .filter_map(|m| segmentation_meta::mask(&meta, m.id()).map(|mask| (m.id(), mask)))
            .collect::<Vec<_>>();
        ensure!(!masks.is_empty(), "{profile}: expected masks");
        for (id, mask) in &masks {
            ensure!(
                mask.map_readable()?.len() == 8,
                "{profile}: source dimensions"
            );
            if encoder.is_some() {
                ensure!(
                    ids.iter()
                        .any(|source| meta.relation(*source, *id)
                            == gst_analytics::RelTypes::RELATE_TO),
                    "{profile}: detection relation"
                );
            }
        }
        if encoder.is_some() {
            ensure!(
                masks.len() == 2,
                "{profile}: both detection prompts retained"
            );
        }
    }
    Ok(())
}

#[test]
fn nonfinite_outputs_fail_the_stream() -> Result<()> {
    initialize()?;
    let e = gst::ElementFactory::make("uslssegment").build()?;
    e.set_property_from_str("model-profile", "birefnet-dis");
    e.set_property("model-file", fixture("nonfinite"));
    let mut h = gst_check::Harness::with_element(&e, Some("sink"), Some("src"));
    h.set_src_caps_str(
        "video/x-raw,format=RGB,width=4,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
    );
    h.play();
    ensure!(
        h.push(gst::Buffer::from_mut_slice(vec![128_u8; 24])) == Err(gst::FlowError::Error),
        "nonfinite inference rejected"
    );
    Ok(())
}

#[test]
fn prompt_budget_fixed_override_and_empty_frames() -> Result<()> {
    initialize()?;
    for (prompt, budget, with_detections, expected) in [
        (None, 8_u32, true, 1),
        (None, 64, false, 0),
        (Some("point:1,1;exclude:3,1"), 64, true, 1),
    ] {
        let e = gst::ElementFactory::make("uslssegment").build()?;
        e.set_property_from_str("model-profile", "mobile-sam");
        e.set_property("encoder-file", fixture("encoder"));
        e.set_property("decoder-file", fixture("sam-decoder"));
        e.set_property("max-mask-bytes", budget);
        e.set_property("prompt", prompt);
        let mut h = gst_check::Harness::with_element(&e, Some("sink"), Some("src"));
        h.set_src_caps_str(
            "video/x-raw,format=RGB,width=4,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
        );
        h.play();
        let mut input = gst::Buffer::from_mut_slice(vec![128_u8; 24]);
        let mut meta = gst_analytics::AnalyticsRelationMeta::add(input.make_mut());
        if with_detections {
            for label in ["first", "second"] {
                meta.add_od_mtd(gst::glib::Quark::from_str(label), 0, 0, 4, 2, 0.9)?;
            }
        }
        let output = h.push_and_pull(input)?;
        let meta = output
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .context("meta")?;
        let masks = meta
            .iter::<gst_analytics::AnalyticsAnyMtd>()
            .filter(|m| segmentation_meta::mask(&meta, m.id()).is_some())
            .collect::<Vec<_>>();
        ensure!(masks.len() == expected, "prompt source and memory budget");
        if prompt.is_some() {
            for object in meta.iter::<gst_analytics::AnalyticsODMtd>() {
                for mask in &masks {
                    ensure!(
                        meta.relation(object.id(), mask.id()).is_empty(),
                        "fixed prompt overrides detections"
                    );
                }
            }
        }
    }
    Ok(())
}
