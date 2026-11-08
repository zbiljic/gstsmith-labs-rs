//! `relationmetatap` on synthetic relation metas.

use anyhow::{Context, Result, ensure};
use gst::glib;
use gst::prelude::*;
use gst_analytics::prelude::*;
use serde_json::{Value, json};

const MS: u64 = 1_000_000;

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

fn frame(pts_ms: u64) -> gst::Buffer {
    let mut buffer = gst::Buffer::new();
    buffer
        .make_mut()
        .set_pts(gst::ClockTime::from_nseconds(pts_ms * MS));
    buffer
}

fn lines(path: &std::path::Path) -> Result<Vec<Value>> {
    std::fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).context("a JSON line"))
        .collect()
}

#[test]
fn records_each_buffer_and_tells_no_meta_from_an_empty_one() -> Result<()> {
    init()?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mtds.jsonl");
    let mut harness =
        gst_check::Harness::new_parse(&format!("relationmetatap location={}", path.display()));
    harness.set_src_caps_str("video/x-raw");
    // A non-zero base, so running time and PTS differ.
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_base(gst::ClockTime::from_seconds(1));
    ensure!(
        harness.push_event(gst::event::Segment::new(&segment)),
        "segment"
    );

    let mut analyzed = frame(0);
    {
        let mut meta = analytics_meta::relation_meta_mut(analyzed.make_mut())?;
        meta.add_od_mtd(glib::Quark::from_str("man"), 10, 10, 50, 80, 0.7)?;
        analytics_meta::schema::add_caption(&mut meta, "a man")?;
    }
    let mut empty = frame(40);
    analytics_meta::relation_meta_mut(empty.make_mut())?;
    for buffer in [analyzed, empty, frame(80)] {
        harness
            .push(buffer)
            .map_err(|err| anyhow::anyhow!("push: {err:?}"))?;
    }
    ensure!(harness.push_event(gst::event::Eos::new()), "EOS");

    let lines = lines(&path)?;
    ensure!(lines.len() == 3, "one line per buffer: {lines:?}");
    let first = lines.first().context("first line")?;
    ensure!(
        first["pts"] == json!(0) && first["running-time"] == json!(1_000 * MS),
        "running time includes the segment base: {first:?}"
    );
    let mtds = first["mtds"].as_array().context("mtds")?;
    ensure!(
        mtds.iter()
            .any(|mtd| mtd["type"] == "object-detection" && mtd["label"] == "man"),
        "the detection: {mtds:?}"
    );
    ensure!(
        mtds.iter()
            .any(|mtd| mtd["schema"] == "x-caption" && mtd["json"]["text"] == "a man"),
        "the caption document: {mtds:?}"
    );
    ensure!(lines.get(1).context("second line")?["mtds"] == json!([]));
    let last = lines.get(2).context("third line")?;
    ensure!(last["pts"] == json!(80 * MS) && last["mtds"].is_null());
    Ok(())
}

#[test]
fn refuses_to_start_without_a_location() -> Result<()> {
    init()?;
    let tap = gst::ElementFactory::make("relationmetatap").build()?;
    ensure!(tap.set_state(gst::State::Paused).is_err());
    tap.set_state(gst::State::Null)?;
    Ok(())
}
