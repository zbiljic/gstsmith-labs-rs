//! `relationmetaevents` on synthetic relation metas.

use anyhow::{Context, Result, ensure};
use gst::glib;
use gst::prelude::*;
use gst_analytics::prelude::*;

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

/// A frame at `pts_ms` with the given labels, or no relation meta at all.
fn frame(pts_ms: u64, labels: Option<&[&str]>) -> Result<gst::Buffer> {
    let mut buffer = gst::Buffer::new();
    let buffer_mut = buffer.make_mut();
    buffer_mut.set_pts(gst::ClockTime::from_nseconds(pts_ms * MS));
    if let Some(labels) = labels {
        let mut meta = analytics_meta::relation_meta_mut(buffer_mut)?;
        for label in labels {
            meta.add_od_mtd(glib::Quark::from_str(*label), 0, 0, 4, 4, 0.8)?;
        }
    }
    Ok(buffer)
}

fn harness(properties: &str) -> Result<gst_check::Harness> {
    init()?;
    let mut harness = gst_check::Harness::new_parse(&format!("relationmetaevents {properties}"));
    harness.set_src_caps_str("video/x-raw");
    Ok(harness)
}

/// Pushes the frames and returns the events they produced.
fn run(harness: &mut gst_check::Harness, frames: Vec<gst::Buffer>) -> Result<Vec<gst::Structure>> {
    for buffer in frames {
        harness
            .push(buffer)
            .map_err(|err| anyhow::anyhow!("push: {err:?}"))?;
        harness
            .pull()
            .map_err(|err| anyhow::anyhow!("pull: {err:?}"))?;
    }
    let mut events = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        if let gst::EventView::CustomDownstream(custom) = event.view()
            && let Some(structure) = custom.structure()
        {
            events.push(structure.to_owned());
        }
    }
    Ok(events)
}

fn summary(events: &[gst::Structure]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            format!(
                "{}:{}:{}",
                event.get::<&str>("type").unwrap_or("?"),
                event.get::<&str>("label").unwrap_or("-"),
                event
                    .get::<u32>("count")
                    .map_or("-".to_owned(), |c| c.to_string()),
            )
        })
        .collect()
}

#[test]
fn registers_with_documented_defaults() -> Result<()> {
    init()?;
    let element = gst::ElementFactory::make("relationmetaevents").build()?;
    ensure!(element.property::<String>("stream-id") == "stream");
    ensure!(element.property::<String>("labels").is_empty());
    ensure!(element.property::<u32>("enter-min-frames") == 1);
    ensure!(element.property::<u64>("leave-min-time") == 0);
    let factory = element.factory().context("factory")?;
    ensure!(factory.metadata("klass") == Some("Filter/Analyzer"));
    Ok(())
}

#[test]
fn reports_presence_counts_and_absence_with_timing() -> Result<()> {
    let mut harness = harness("stream-id=cam1 leave-min-time=100000000")?;
    let events = run(
        &mut harness,
        vec![
            frame(0, Some(&["man"]))?,
            frame(40, Some(&["man", "man"]))?,
            frame(80, Some(&[]))?,
            frame(120, Some(&[]))?,
            frame(200, Some(&[]))?,
        ],
    )?;
    ensure!(
        summary(&events) == ["present:man:1", "count-changed:man:2", "absent:man:0"],
        "{events:?}"
    );
    let first = events.first().context("first event")?;
    ensure!(first.name() == "relationmeta-event");
    ensure!(first.get::<&str>("stream-id")? == "cam1");
    ensure!(first.get::<u64>("pts")? == 0 && first.get::<u64>("running-time")? == 0);
    ensure!((first.get::<f64>("score")? - 0.8).abs() < 1e-6);
    ensure!(!first.has_field("analyzed-pts"));
    // Absence starts at 80 ms and is reported once 100 ms have passed.
    ensure!(events.last().context("last")?.get::<u64>("pts")? == 200 * MS);
    Ok(())
}

#[test]
fn buffers_without_relation_meta_are_not_observations() -> Result<()> {
    let mut harness = harness("")?;
    let events = run(
        &mut harness,
        vec![
            frame(0, Some(&["cup"]))?,
            frame(40, None)?,
            frame(80, None)?,
        ],
    )?;
    ensure!(summary(&events) == ["present:cup:1"], "{events:?}");
    Ok(())
}

#[test]
fn filters_by_label_and_event_type() -> Result<()> {
    let mut harness = harness("labels=cup events=absent")?;
    let events = run(
        &mut harness,
        vec![frame(0, Some(&["man", "cup"]))?, frame(40, Some(&[]))?],
    )?;
    ensure!(summary(&events) == ["absent:cup:0"], "{events:?}");
    Ok(())
}

#[test]
fn reports_the_analyzed_frame_from_provenance() -> Result<()> {
    let mut harness = harness("")?;
    let mut buffer = frame(500, Some(&["man"]))?;
    {
        let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut())?;
        analytics_meta::schema::add_provenance(
            &mut meta,
            &analytics_meta::schema::Provenance {
                pts: Some(320 * MS),
                running_time: None,
                producer: "test".to_owned(),
            },
        )?;
    }
    let events = run(&mut harness, vec![buffer])?;
    let event = events.first().context("event")?;
    ensure!(event.get::<u64>("analyzed-pts")? == 320 * MS);
    ensure!(event.get::<u64>("pts")? == 500 * MS);
    Ok(())
}

#[test]
fn a_flush_forgets_the_state() -> Result<()> {
    let mut harness = harness("")?;
    let events = run(&mut harness, vec![frame(0, Some(&["man"]))?])?;
    ensure!(summary(&events) == ["present:man:1"]);
    ensure!(harness.push_event(gst::event::FlushStart::new()));
    ensure!(harness.push_event(gst::event::FlushStop::new(true)));
    ensure!(
        harness.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    // After the flush the man is new again, and no absence is reported.
    let events = run(&mut harness, vec![frame(0, Some(&["man"]))?])?;
    ensure!(summary(&events) == ["present:man:1"], "{events:?}");
    Ok(())
}

#[test]
fn a_new_segment_keeps_presence() -> Result<()> {
    let mut harness = harness("")?;
    let events = run(&mut harness, vec![frame(0, Some(&["man"]))?])?;
    ensure!(summary(&events) == ["present:man:1"]);
    ensure!(
        harness.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    let events = run(&mut harness, vec![frame(0, Some(&["man"]))?])?;
    ensure!(
        events.is_empty(),
        "present again after a segment: {events:?}"
    );
    Ok(())
}

/// A result attached again to later frames, as producers with a hold time do.
fn held(pts_ms: u64, analyzed_ms: u64, labels: &[&str]) -> Result<gst::Buffer> {
    let mut buffer = frame(pts_ms, Some(labels))?;
    {
        let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut())?;
        analytics_meta::schema::add_provenance(
            &mut meta,
            &analytics_meta::schema::Provenance {
                pts: Some(analyzed_ms * MS),
                running_time: None,
                producer: "test".to_owned(),
            },
        )?;
    }
    Ok(buffer)
}

#[test]
fn a_held_result_counts_as_one_analyzed_frame() -> Result<()> {
    let mut harness = harness("enter-min-frames=2")?;
    let events = run(
        &mut harness,
        vec![
            held(0, 0, &["man"])?,
            held(40, 0, &["man"])?,
            held(80, 0, &["man"])?,
        ],
    )?;
    ensure!(events.is_empty(), "one inference entered: {events:?}");
    let events = run(&mut harness, vec![held(120, 100, &["man"])?])?;
    ensure!(summary(&events) == ["present:man:1"], "{events:?}");
    Ok(())
}
