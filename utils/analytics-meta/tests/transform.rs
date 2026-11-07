//! Documents and relation edges survive upstream's relation-meta transforms:
//! copies made when a buffer becomes writable, and the matrix path that
//! `videoconvertscale` uses. `ODMtd` boxes are rescaled; `x-*` documents are
//! copied unchanged, which is why they hold only normalized coordinates.

use analytics_meta::{json, schema};
use anyhow::{Context, Result, ensure};
use gst::glib;
use gst_analytics::prelude::*;
use gst_analytics::{AnalyticsODMtd, AnalyticsRelationMeta};

const CAPS: &str = "video/x-raw,format=RGB,width=64,height=48,framerate=30/1";

fn polygon() -> schema::Polygon {
    schema::Polygon {
        rings: vec![vec![0.25, 0.25, 0.5, 0.25, 0.5, 0.5]],
    }
}

/// A frame with a decoy `ODMtd` first, so ids are not trivially stable.
fn frame() -> Result<gst::Buffer> {
    gst::init()?;
    let mut buffer = gst::Buffer::with_size(64 * 48 * 3)?;
    {
        let buffer = buffer.make_mut();
        buffer.set_pts(gst::ClockTime::from_mseconds(40));
        let mut meta = analytics_meta::relation_meta_mut(buffer)?;
        meta.add_od_mtd(glib::Quark::from_str("decoy"), 0, 0, 2, 2, 0.1)?;
        let man = meta
            .add_od_mtd(glib::Quark::from_str("man"), 8, 8, 16, 24, 0.7)?
            .id();
        let cup = meta
            .add_od_mtd(glib::Quark::from_str("cup"), 32, 16, 8, 8, 0.6)?
            .id();
        schema::add_polygon(&mut meta, man, &polygon())?;
        schema::add_relation(&mut meta, man, cup, "holding")?;
        schema::add_caption(&mut meta, "a man holding a cup")?;
        json::add(&mut meta, "example/detector/record", r#"{"objects":[]}"#)?;
    }
    Ok(buffer)
}

/// `GStreamer` 1.28.0 to 1.28.2 drop `ODMtd`s, and with them their edges, when
/// a buffer is copied; 1.28.3 fixed it.
fn copies_keep_boxes() -> bool {
    gst::version() >= (1, 28, 3, 0)
}

fn label_of(meta: &gst::MetaRef<'_, AnalyticsRelationMeta>, id: u32) -> Option<String> {
    meta.mtd::<AnalyticsODMtd>(id)?
        .obj_type()
        .map(|quark| quark.as_str().to_string())
}

/// Checks documents and edges, and returns the `man` box.
fn check(buffer: &gst::BufferRef) -> Result<(i32, i32, i32, i32)> {
    let meta = buffer
        .meta::<AnalyticsRelationMeta>()
        .context("relation meta")?;
    let man = meta
        .iter::<AnalyticsODMtd>()
        .find(|od| od.obj_type().is_some_and(|q| q.as_str() == "man"))
        .context("man")?;
    let location = man.location()?;
    ensure!(
        schema::polygons(&meta, man.id()) == vec![polygon()],
        "polygon"
    );
    let relations = schema::relations(&meta);
    let [edge] = relations.as_slice() else {
        anyhow::bail!("relations: {relations:?}");
    };
    ensure!(
        edge.predicate == "holding" && edge.subject == man.id(),
        "{edge:?}"
    );
    ensure!(
        label_of(&meta, edge.object).as_deref() == Some("cup"),
        "{edge:?}"
    );
    ensure!(
        meta.relation(edge.subject, edge.object) == gst_analytics::RelTypes::RELATE_TO,
        "direct edge"
    );
    ensure!(
        schema::caption(&meta).map(|c| c.text).as_deref() == Some("a man holding a cup"),
        "caption"
    );
    let records: Vec<_> = json::documents_with_schema(&meta, "example/detector/record")
        .map(|doc| doc.json)
        .collect();
    ensure!(records == [r#"{"objects":[]}"#], "records {records:?}");
    Ok((location.x, location.y, location.w, location.h))
}

fn run(pipeline: &str, input: gst::Buffer) -> Result<gst::Buffer> {
    let mut harness = gst_check::Harness::new_parse(pipeline);
    harness.set_src_caps_str(CAPS);
    harness
        .push_and_pull(input)
        .map_err(|err| anyhow::anyhow!("{pipeline}: {err:?}"))
}

#[test]
fn writable_copy_keeps_documents_and_edges() -> Result<()> {
    let original = frame()?;
    if !copies_keep_boxes() {
        let copy = original.copy();
        let meta = copy.meta::<AnalyticsRelationMeta>().context("meta")?;
        ensure!(
            json::documents(&meta).count() == 4,
            "documents survive the copy"
        );
        eprintln!(
            "{} drops ODMtds on copy; checked documents only",
            gst::version_string()
        );
        return Ok(());
    }
    let mut copy = original.copy();
    // `copy` runs the relation meta's copy transform into a new buffer.
    ensure!(check(copy.make_mut())? == (8, 8, 16, 24));
    ensure!(check(&original)? == (8, 8, 16, 24));
    let deep = original.copy_deep()?;
    drop(original);
    ensure!(check(&deep)? == (8, 8, 16, 24));
    Ok(())
}

#[test]
fn scaling_rescales_boxes_and_keeps_documents() -> Result<()> {
    let output = run(
        r#"videoconvertscale ! capsfilter caps="video/x-raw,width=128,height=96""#,
        frame()?,
    )?;
    ensure!(check(&output)? == (16, 16, 32, 48));
    Ok(())
}

#[test]
fn format_conversion_keeps_documents() -> Result<()> {
    let output = run(
        r#"videoconvert ! capsfilter caps="video/x-raw,format=I420""#,
        frame()?,
    )?;
    ensure!(check(&output)? == (8, 8, 16, 24));
    Ok(())
}
