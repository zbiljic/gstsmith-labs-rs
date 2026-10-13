#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

use gst::prelude::*;
use serde_json::Value;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstjsontap::plugin_register_static().expect("registering the jsontap plugin");
    });
}

fn records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("records file exists")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect()
}

#[test]
fn registers_the_element() {
    init();
    assert!(gst::ElementFactory::find("jsontap").is_some());
}

#[test]
fn records_caps_buffers_and_eos_and_passes_data_through() {
    init();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("tap.jsonl");
    let mut h = gst_check::Harness::new_parse(&format!("jsontap location={}", path.display()));
    h.set_src_caps_str("application/x-test");

    let mut input = gst::Buffer::from_slice(vec![9u8, 8, 7]);
    input
        .get_mut()
        .expect("writable")
        .set_pts(gst::ClockTime::from_mseconds(5));
    let original = input.clone();
    h.push(input).expect("buffer pushes");
    let out = h.pull().expect("buffer passes through");
    assert_eq!(out.as_ptr(), original.as_ptr());
    assert_eq!(out.map_readable().expect("readable").as_slice(), &[9, 8, 7]);
    assert_eq!(out.pts(), Some(gst::ClockTime::from_mseconds(5)));
    h.push(gst::Buffer::with_size(2).expect("buffer"))
        .expect("buffer pushes");
    h.pull().expect("buffer passes through");
    assert!(h.push_event(gst::event::Eos::new()));

    let records = records(&path);
    let kinds = records
        .iter()
        .map(|r| r["kind"].as_str().expect("kind").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["caps", "buffer", "buffer", "eos"]);
    assert_eq!(records[0]["caps"], "application/x-test");
    assert_eq!(records[1]["seq"], 0);
    assert_eq!(records[1]["pts"], 5_000_000u64);
    assert_eq!(records[1]["size"], 3);
    assert_eq!(records[2]["seq"], 1);
}

#[test]
fn stopping_without_eos_flushes_buffered_records() {
    init();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("tap.jsonl");
    {
        let mut h = gst_check::Harness::new_parse(&format!("jsontap location={}", path.display()));
        h.set_src_caps_str("application/x-test");
        h.push(gst::Buffer::new()).expect("buffer pushes");
        h.pull().expect("buffer passes through");
    }
    let records = records(&path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["kind"], "caps");
    assert_eq!(records[1]["kind"], "buffer");
}

#[test]
fn restarting_changes_location_and_resets_sequence_numbers() {
    init();
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first.jsonl");
    let second = dir.path().join("second.jsonl");
    let tap = gst::ElementFactory::make("jsontap")
        .property("location", first.to_str().expect("UTF-8 path"))
        .build()
        .expect("tap");
    let mut h = gst_check::Harness::with_element(&tap, Some("sink"), Some("src"));
    h.set_src_caps_str("application/x-test");
    h.push(gst::Buffer::new()).expect("buffer pushes");
    h.pull().expect("buffer passes through");

    tap.set_state(gst::State::Ready).expect("ready");
    assert_eq!(records(&first).len(), 2);
    tap.set_property("location", second.to_str().expect("UTF-8 path"));
    tap.set_state(gst::State::Playing).expect("playing");
    assert!(h.push_event(gst::event::StreamStart::new("restarted")));
    h.set_src_caps_str("application/x-test");
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    h.push(gst::Buffer::new()).expect("buffer pushes");
    h.pull().expect("buffer passes through");
    assert!(h.push_event(gst::event::Eos::new()));

    let first = records(&first);
    let second = records(&second);
    assert_eq!(first[1]["seq"], 0);
    assert_eq!(second.len(), 3);
    assert_eq!(second[0]["kind"], "caps");
    assert_eq!(second[1]["seq"], 0);
    assert_eq!(second[2]["kind"], "eos");
}

#[test]
fn pull_mode_preserves_the_buffer_and_flushes_on_stop() {
    init();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("pull.jsonl");
    let tap = gst::ElementFactory::make("jsontap")
        .property("location", path.to_str().expect("UTF-8 path"))
        .build()
        .expect("tap");
    let sink = tap.static_pad("sink").expect("sink pad");
    let src = tap.static_pad("src").expect("source pad");
    let mut input = gst::Buffer::from_slice(vec![9u8, 8, 7]);
    input
        .get_mut()
        .expect("writable")
        .set_pts(gst::ClockTime::from_mseconds(5));
    let original = input.clone();
    let upstream = gst::Pad::builder(gst::PadDirection::Src)
        .getrange_function(move |_, _, _, _, _| {
            Ok(gst::PadGetRangeSuccess::NewBuffer(input.clone()))
        })
        .build();
    upstream.link(&sink).expect("link source");
    src.activate_mode(gst::PadMode::Pull, true)
        .expect("activate pull mode");
    assert!(sink.send_event(gst::event::StreamStart::new("pull-test")));
    assert!(sink.send_event(gst::event::Caps::new(
        &gst::Caps::builder("application/x-test").build()
    )));
    assert!(
        sink.send_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );

    let output = src.range(0, 3).expect("pulled buffer");
    assert_eq!(output.as_ptr(), original.as_ptr());
    src.activate_mode(gst::PadMode::Pull, false)
        .expect("deactivate pull mode");
    upstream.unlink(&sink).expect("unlink source");
    let records = records(&path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["kind"], "caps");
    assert_eq!(records[1]["kind"], "buffer");
    assert_eq!(records[1]["seq"], 0);
    assert_eq!(records[1]["pts"], 5_000_000u64);
    assert_eq!(records[1]["size"], 3);
}

#[test]
fn fails_to_start_without_a_location() {
    init();
    let tap = gst::ElementFactory::make("jsontap")
        .build()
        .expect("jsontap builds");
    let _err = tap
        .set_state(gst::State::Paused)
        .expect_err("start requires a location");
    tap.set_state(gst::State::Null).expect("tap stops");
}
