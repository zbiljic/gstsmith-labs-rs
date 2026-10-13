#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

use gst::prelude::*;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstptssampler::plugin_register_static().expect("registering the ptssampler plugin");
    });
}

const FRAME_MS: u64 = 33;

fn harness(properties: &str) -> gst_check::Harness {
    init();
    let mut h = gst_check::Harness::new_parse(&format!("ptssampler {properties}"));
    h.set_src_caps_str("application/x-test");
    h
}

fn buffer(pts_ms: Option<u64>) -> gst::Buffer {
    let mut buffer = gst::Buffer::with_size(1).expect("buffer");
    buffer
        .get_mut()
        .expect("writable")
        .set_pts(pts_ms.map(gst::ClockTime::from_mseconds));
    buffer
}

/// Push buffers at the given PTS values; return the PTS values that passed.
fn kept(h: &mut gst_check::Harness, pts_ms: impl IntoIterator<Item = u64>) -> Vec<u64> {
    for pts in pts_ms {
        h.push(buffer(Some(pts))).expect("buffer pushes");
    }
    std::iter::from_fn(|| h.try_pull())
        .map(|b| b.pts().expect("pts").mseconds())
        .collect()
}

#[test]
fn registers_the_element() {
    init();
    assert!(gst::ElementFactory::find("ptssampler").is_some());
}

#[test]
fn keeps_the_first_buffer_of_every_period() {
    let mut h = harness("period=150000000");
    let frames = (0..12).map(|i| i * FRAME_MS);
    assert_eq!(kept(&mut h, frames), [0, 165, 330]);
}

#[test]
fn phase_shifts_period_boundaries() {
    let mut h = harness("period=150000000 phase=50000000");
    let frames = (0..12).map(|i| i * FRAME_MS);
    assert_eq!(kept(&mut h, frames), [0, 66, 231, 363]);
}

#[test]
fn zero_period_passes_everything() {
    let mut h = harness("period=0");
    let frames = (0..4).map(|i| i * FRAME_MS);
    assert_eq!(kept(&mut h, frames), [0, 33, 66, 99]);
}

#[test]
fn buffers_without_timestamps_pass_through() {
    let mut h = harness("period=150000000");
    h.push(buffer(Some(0))).expect("buffer pushes");
    h.push(buffer(None)).expect("buffer pushes");
    h.push(buffer(Some(33))).expect("buffer pushes");
    let out = std::iter::from_fn(|| h.try_pull())
        .map(|b| b.pts().map(gst::ClockTime::mseconds))
        .collect::<Vec<_>>();
    assert_eq!(out, [Some(0), None]);
}

#[test]
fn a_new_segment_starts_sampling_again() {
    let mut h = harness("period=150000000");
    assert_eq!(kept(&mut h, [0, 33]), [0]);
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(kept(&mut h, [33, 66]), [33]);
}

#[test]
fn samples_running_time_with_segment_start_base_and_rate() {
    let mut h = harness("period=150000000");
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_start(gst::ClockTime::from_mseconds(1_000));
    segment.set_base(gst::ClockTime::from_mseconds(50));
    segment.set_rate(2.0);
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(
        kept(&mut h, [1_000, 1_150, 1_200, 1_450, 1_500]),
        [1_000, 1_200, 1_500]
    );
}

#[test]
fn repeated_and_older_periods_do_not_reopen_sampling() {
    let mut h = harness("period=150000000");
    assert_eq!(kept(&mut h, [300, 300, 0, 150, 449, 450]), [300, 450]);
}

#[test]
fn reverse_playback_samples_in_running_time_order() {
    let mut h = harness("period=150000000");
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_stop(gst::ClockTime::from_mseconds(1_000));
    segment.set_base(gst::ClockTime::from_mseconds(50));
    segment.set_rate(-2.0);
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(
        kept(&mut h, [1_000, 900, 800, 750, 500, 0]),
        [1_000, 800, 500, 0]
    );
}

#[test]
fn non_time_segments_pass_through_and_do_not_reuse_the_time_segment() {
    let mut h = harness("period=150000000");
    assert_eq!(kept(&mut h, [0, 33]), [0]);
    let segment = gst::FormattedSegment::<gst::format::Bytes>::new();
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(kept(&mut h, [33, 66]), [33, 66]);
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(kept(&mut h, [33, 66]), [33]);
}

#[test]
fn stream_start_and_flush_stop_restart_sampling() {
    let mut h = harness("period=150000000");
    assert_eq!(kept(&mut h, [0, 33]), [0]);
    assert!(h.push_event(gst::event::StreamStart::new("replacement")));
    assert_eq!(kept(&mut h, [33, 66]), [33]);
    assert!(h.push_event(gst::event::FlushStart::new()));
    assert!(h.push_event(gst::event::FlushStop::new(false)));
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    assert_eq!(kept(&mut h, [66, 99]), [66]);
}

#[test]
fn ready_property_changes_take_effect_after_restarting() {
    let mut h = harness("period=150000000");
    assert_eq!(kept(&mut h, [0, 33]), [0]);
    let element = h.find_element("ptssampler").expect("sampler");
    for (period, phase, expected) in [
        (0u64, 0u64, vec![0, 33, 66, 99]),
        (50_000_000, 25_000_000, vec![0, 33, 99]),
    ] {
        element.set_state(gst::State::Ready).expect("ready");
        element.set_property("period", period);
        element.set_property("phase", phase);
        element.set_state(gst::State::Playing).expect("playing");
        assert!(h.push_event(gst::event::StreamStart::new("restarted")));
        h.set_src_caps_str("application/x-test");
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        assert!(h.push_event(gst::event::Segment::new(&segment)));
        assert_eq!(kept(&mut h, [0, 33, 66, 99]), expected);
    }
}

#[test]
fn sampling_and_passthrough_instances_are_independent() {
    let mut disabled = harness("period=0");
    let mut enabled = harness("period=150000000");
    assert_eq!(kept(&mut disabled, [0, 33, 66, 165]), [0, 33, 66, 165]);
    assert_eq!(kept(&mut enabled, [0, 33, 66, 165]), [0, 165]);
}

#[test]
fn serialized_events_and_property_reads_can_overlap_streaming() {
    let mut h = harness("period=150000000 phase=50000000");
    let element = h.find_element("ptssampler").expect("sampler");
    let sink = element.static_pad("sink").expect("sink pad");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            for _ in 0..200 {
                assert_eq!(element.property::<u64>("period"), 150_000_000);
                assert_eq!(element.property::<u64>("phase"), 50_000_000);
                let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                assert!(sink.send_event(gst::event::Segment::new(&segment)));
                std::thread::yield_now();
            }
        });
        barrier.wait();
        for pts in 0..2_000 {
            let input = buffer(Some(pts));
            h.push(input.clone()).expect("buffer pushes");
            if let Some(output) = h.try_pull() {
                assert_eq!(
                    input.peek_memory(0).as_ptr(),
                    output.peek_memory(0).as_ptr()
                );
                assert_eq!(output.pts(), Some(gst::ClockTime::from_mseconds(pts)));
            }
        }
    });
}

#[test]
fn switching_pad_modes_preserves_sampling_and_pull_synchronization() {
    init();
    let sampler = gst::ElementFactory::make("ptssampler")
        .build()
        .expect("sampler");
    {
        let mut h = gst_check::Harness::with_element(&sampler, Some("sink"), Some("src"));
        h.set_src_caps_str("application/x-test");
        assert_eq!(kept(&mut h, [0, 33]), [0]);
    }
    let sink = sampler.static_pad("sink").expect("sink pad");
    let src = sampler.static_pad("src").expect("source pad");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = barrier.clone();
    let upstream = gst::Pad::builder(gst::PadDirection::Src)
        .getrange_function(move |_, _, _, _, _| {
            entered_tx.send(()).expect("signal source callback");
            release.wait();
            Ok(gst::PadGetRangeSuccess::NewBuffer(buffer(Some(0))))
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

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let timeout = std::time::Duration::from_secs(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            result_tx.send(src.range(0, 1)).expect("send pulled buffer");
        });
        entered_rx
            .recv_timeout(timeout)
            .expect("source callback reached");
        let guard = sink.stream_lock();
        barrier.wait();
        assert!(matches!(
            result_rx.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(guard);
        let output = result_rx
            .recv_timeout(timeout)
            .expect("pull completed")
            .expect("buffer");
        assert_eq!(output.pts(), Some(gst::ClockTime::ZERO));
    });
    src.activate_mode(gst::PadMode::Pull, false)
        .expect("deactivate pull mode");
    upstream.unlink(&sink).expect("unlink source");
    let mut h = gst_check::Harness::with_element(&sampler, Some("sink"), Some("src"));
    h.set_src_caps_str("application/x-test");
    assert_eq!(kept(&mut h, [0, 33]), [0]);
}
