//! `relationmetaoverlay` on synthetic relation metas.

use anyhow::{Context, Result, ensure};
use gst::glib;
use gst::prelude::*;
use gst_analytics::prelude::*;

const MS: u64 = 1_000_000;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

fn init() -> Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    let mut result = Ok(());
    INIT.call_once(|| {
        result = gst::init().and_then(|()| {
            gstrelationmeta::plugin_register_static()
                .map_err(|err| glib::Error::new(gst::CoreError::Failed, &err.to_string()))
        });
    });
    Ok(result?)
}

/// A black BGRA frame at `pts_ms`, with a `man` box when `detect` is set.
fn frame(pts_ms: u64, detect: bool) -> Result<gst::Buffer> {
    let size = usize::try_from(WIDTH * HEIGHT * 4)?;
    let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; size]);
    let buffer_mut = buffer.make_mut();
    buffer_mut.set_pts(gst::ClockTime::from_nseconds(pts_ms * MS));
    if detect {
        let mut meta = analytics_meta::relation_meta_mut(buffer_mut)?;
        meta.add_od_mtd(glib::Quark::from_str("man"), 10, 20, 30, 20, 0.7)?;
    }
    Ok(buffer)
}

fn harness(properties: &str, format: &str) -> Result<gst_check::Harness> {
    init()?;
    let mut harness = gst_check::Harness::new_parse(&format!("relationmetaoverlay {properties}"));
    harness.set_src_caps_str(&format!(
        "video/x-raw,format={format},width={WIDTH},height={HEIGHT},framerate=30/1"
    ));
    Ok(harness)
}

fn push(harness: &mut gst_check::Harness, buffer: gst::Buffer) -> Result<gst::Buffer> {
    harness
        .push_and_pull(buffer)
        .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))
}

/// The BGRA pixel at `x`, `y`.
fn pixel(buffer: &gst::BufferRef, x: u32, y: u32) -> Result<[u8; 4]> {
    let map = buffer.map_readable()?;
    let offset = usize::try_from((y * WIDTH + x) * 4)?;
    let bytes = map.get(offset..offset + 4).context("pixel in range")?;
    Ok(bytes.try_into()?)
}

#[test]
fn registers_with_documented_defaults() -> Result<()> {
    init()?;
    let element = gst::ElementFactory::make("relationmetaoverlay").build()?;
    ensure!(element.property::<bool>("draw-boxes") && !element.property::<bool>("draw-text"));
    ensure!(element.property::<String>("font-desc") == "monospace 14");
    ensure!(element.property::<u64>("expire-overlay") == 1_000_000_000);
    let factory = element.factory().context("factory")?;
    ensure!(factory.metadata("klass") == Some("Filter/Editor/Video"));
    Ok(())
}

#[test]
fn blends_boxes_into_the_frame() -> Result<()> {
    let mut harness = harness("draw-labels=false", "BGRA")?;
    let output = push(&mut harness, frame(0, true)?)?;
    ensure!(pixel(&output, 20, 20)? != [0, 0, 0, 0], "box edge is drawn");
    ensure!(
        pixel(&output, 25, 30)? == [0, 0, 0, 0],
        "box inside stays black"
    );
    ensure!(
        output
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .is_none()
    );
    Ok(())
}

#[test]
fn attaches_a_composition_when_downstream_renders_it() -> Result<()> {
    // Caps first: the element asks downstream only once buffers flow.
    let mut harness = harness("", "BGRA")?;
    harness.add_propose_allocation_meta(gst_video::VideoOverlayCompositionMeta::meta_api(), None);
    let output = push(&mut harness, frame(0, true)?)?;
    let meta = output
        .meta::<gst_video::VideoOverlayCompositionMeta>()
        .context("composition meta")?;
    ensure!(meta.overlay().n_rectangles() == 1);
    ensure!(
        pixel(&output, 20, 20)? == [0, 0, 0, 0],
        "frame left untouched"
    );
    Ok(())
}

#[test]
fn an_unchanged_result_reuses_its_composition() -> Result<()> {
    let mut harness = harness("", "BGRA")?;
    harness.add_propose_allocation_meta(gst_video::VideoOverlayCompositionMeta::meta_api(), None);
    let seqnum = |buffer: &gst::Buffer| -> Result<u32> {
        Ok(buffer
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .context("composition meta")?
            .overlay()
            .seqnum())
    };
    let first = seqnum(&push(&mut harness, frame(0, true)?)?)?;
    let again = seqnum(&push(&mut harness, frame(40, true)?)?)?;
    ensure!(first == again, "an identical result was rendered again");
    Ok(())
}

#[test]
fn keeps_the_last_result_until_it_expires() -> Result<()> {
    let mut harness = harness("draw-labels=false expire-overlay=500000000", "BGRA")?;
    push(&mut harness, frame(0, true)?)?;
    let held = push(&mut harness, frame(400, false)?)?;
    ensure!(pixel(&held, 20, 20)? != [0, 0, 0, 0], "drawn within 500 ms");
    let expired = push(&mut harness, frame(600, false)?)?;
    ensure!(
        pixel(&expired, 20, 20)? == [0, 0, 0, 0],
        "gone after 500 ms"
    );
    Ok(())
}

#[test]
fn a_result_without_objects_clears_the_overlay() -> Result<()> {
    let mut harness = harness("draw-labels=false", "BGRA")?;
    push(&mut harness, frame(0, true)?)?;
    let mut empty = frame(40, false)?;
    analytics_meta::relation_meta_mut(empty.make_mut())?;
    let cleared = push(&mut harness, empty)?;
    ensure!(pixel(&cleared, 20, 20)? == [0, 0, 0, 0]);
    Ok(())
}

#[test]
fn blends_into_yuv_frames() -> Result<()> {
    let mut harness = harness("", "I420")?;
    let size = usize::try_from(WIDTH * HEIGHT * 3 / 2)?;
    let mut buffer = gst::Buffer::from_mut_slice(vec![16u8; size]);
    {
        let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut())?;
        meta.add_od_mtd(glib::Quark::from_str("cup"), 10, 20, 30, 20, 0.7)?;
    }
    let output = push(&mut harness, buffer)?;
    let map = output.map_readable()?;
    let luma = map.get(usize::try_from(20 * WIDTH + 20)?).copied();
    ensure!(luma.is_some_and(|y| y > 16), "the box edge brightens luma");
    Ok(())
}

/// Writes a demo-style frame to the PNG named by `RELATIONMETA_DEMO_PNG`, for
/// looking at the style by eye:
/// `RELATIONMETA_DEMO_PNG=/tmp/demo.png cargo test -p gst-plugin-relationmeta --test relationmetaoverlay demo -- --ignored`
#[test]
#[ignore = "writes a PNG for visual review"]
fn demo_frame_png() -> Result<()> {
    use analytics_meta::schema;
    init()?;
    let path = std::env::var("RELATIONMETA_DEMO_PNG").context("RELATIONMETA_DEMO_PNG")?;
    let (width, height) = (640u32, 360u32);
    let pixels = [40u8, 40, 40, 255].repeat(usize::try_from(width * height)?);
    let mut buffer = gst::Buffer::from_mut_slice(pixels);
    {
        let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut())?;
        let man = meta
            .add_od_mtd(glib::Quark::from_str("man"), 120, 60, 160, 280, 0.69)?
            .id();
        let cup = meta
            .add_od_mtd(glib::Quark::from_str("cup"), 300, 200, 40, 50, 0.81)?
            .id();
        meta.add_od_mtd(
            glib::Quark::from_str("framed picture"),
            140,
            70,
            90,
            60,
            0.55,
        )?;
        meta.add_od_mtd(glib::Quark::from_str("lamp"), 470, 40, 80, 120, 0.42)?;
        let line = meta
            .add_od_mtd(glib::Quark::from_str("text-line"), 420, 320, 180, 24, 0.9)?
            .id();
        schema::add_polygon(
            &mut meta,
            man,
            &schema::Polygon {
                rings: vec![vec![
                    0.22, 0.2, 0.4, 0.18, 0.42, 0.6, 0.38, 0.93, 0.22, 0.93, 0.2, 0.5,
                ]],
            },
        )?;
        schema::add_relation(&mut meta, man, cup, "holding")?;
        schema::add_text(
            &mut meta,
            line,
            &schema::Text {
                string: "2026-01-30 05:21:35".to_owned(),
                score: Some(0.9),
            },
        )?;
        schema::add_caption(&mut meta, "room: a man holding a cup next to a lamp")?;
    }
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src caps=video/x-raw,format=BGRA,width={width},height={height},framerate=1/1 \
         ! relationmetaoverlay draw-text=true draw-caption=true ! videoconvert ! pngenc \
         ! filesink location={path}"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_err| anyhow::anyhow!("not a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .context("appsrc")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_err| anyhow::anyhow!("not an appsrc"))?;
    pipeline.set_state(gst::State::Playing)?;
    src.push_buffer(buffer)?;
    src.end_of_stream()?;
    let bus = pipeline.bus().context("bus")?;
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .context("no EOS")?;
    pipeline.set_state(gst::State::Null)?;
    ensure!(
        matches!(message.view(), gst::MessageView::Eos(_)),
        "{message:?}"
    );
    Ok(())
}
