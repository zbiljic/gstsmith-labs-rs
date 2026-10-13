#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

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
