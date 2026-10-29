#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

use gst::glib;
use gst::prelude::*;
use gst_video::prelude::*;
use wall_geometry::{Layout, Point, Quad, Screen};

const LAYOUT: &str = include_str!("../examples/three-screens.json");

type Rows = Vec<Vec<[u8; 4]>>;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstwallwarp::plugin_register_static().expect("registering the wallwarp plugin");
    });
}

fn caps(width: u32, height: u32) -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(i32::try_from(width).expect("width"))
        .height(i32::try_from(height).expect("height"))
        .framerate(gst::Fraction::new(30, 1))
        .build()
}

/// An opaque color encoding pixel coordinates below 4096.
fn encode(x: u32, y: u32) -> [u8; 4] {
    let [x_low, x_high, ..] = x.to_le_bytes();
    let [y_low, y_high, ..] = y.to_le_bytes();
    [x_low, y_low, x_high | (y_high << 4), 0xff]
}

/// The source pixel an output pixel sampled, or `None` for background.
fn decode([r, g, b, a]: [u8; 4]) -> Option<(u32, u32)> {
    (a == 0xff).then(|| {
        (
            u32::from(r) | (u32::from(b & 0x0f) << 8),
            u32::from(g) | (u32::from(b >> 4) << 8),
        )
    })
}

/// A frame whose pixels encode their coordinates. Padding and a leading
/// offset are described by a video meta.
fn grid(width: u32, height: u32, padding: usize, offset: usize) -> gst::Buffer {
    let stride = width as usize * 4 + padding;
    let mut data = vec![0xee; offset + stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let start = offset + y as usize * stride + x as usize * 4;
            data.get_mut(start..start + 4)
                .expect("pixel")
                .copy_from_slice(&encode(x, y));
        }
    }
    let mut buffer = gst::Buffer::from_mut_slice(data);
    if padding > 0 || offset > 0 {
        gst_video::VideoMeta::add_full(
            buffer.get_mut().expect("writable"),
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgba,
            width,
            height,
            &[offset],
            &[i32::try_from(stride).expect("stride")],
        )
        .expect("video meta");
    }
    buffer
}

fn pixels(buffer: &gst::BufferRef, caps: &gst::CapsRef) -> Rows {
    let info = gst_video::VideoInfo::from_caps(caps).expect("video caps");
    let frame =
        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("readable frame");
    let stride = frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
        .expect("positive stride");
    let row = info.width() as usize * 4;
    frame
        .plane_data(0)
        .expect("plane")
        .chunks(stride)
        .take(info.height() as usize)
        .map(|data| data.get(..row).expect("row").as_chunks::<4>().0.to_vec())
        .collect()
}

fn harness(properties: &str, input: (u32, u32)) -> gst_check::Harness {
    init();
    let mut h = gst_check::Harness::new_parse(&format!("wallwarp name=warp {properties}"));
    h.set_src_caps(caps(input.0, input.1));
    h
}

fn element(h: &gst_check::Harness) -> gst::Element {
    h.element()
        .expect("harness element")
        .downcast::<gst::Bin>()
        .expect("parsed bin")
        .by_name("warp")
        .expect("wallwarp")
}

fn output_caps(h: &gst_check::Harness) -> gst::Caps {
    h.sinkpad()
        .expect("sink pad")
        .current_caps()
        .expect("negotiated caps")
}

/// Push a frame and return the output pixels.
fn warp(h: &mut gst_check::Harness, buffer: gst::Buffer) -> Rows {
    h.push(buffer).expect("pushing a frame");
    let output = h.pull().expect("output frame");
    pixels(&output, &output_caps(h))
}

fn drain_events(h: &mut gst_check::Harness) -> Vec<gst::Event> {
    std::iter::from_fn(|| h.try_pull_event()).collect()
}

fn size(rows: &Rows) -> (usize, usize) {
    (rows.first().map_or(0, Vec::len), rows.len())
}

/// Assert every output pixel `(x, y)` sampled `expected(x, y)`.
fn assert_samples(rows: &Rows, expected: impl Fn(u32, u32) -> Option<(u32, u32)>) {
    for (y, row) in (0..).zip(rows) {
        for (x, pixel) in (0..).zip(row) {
            assert_eq!(decode(*pixel), expected(x, y), "output pixel ({x}, {y})");
        }
    }
}

const INTERPOLATIONS: [&str; 2] = ["nearest", "bilinear"];

/// A quad, its output size, and the source pixel each output pixel shows.
type Turn = (&'static str, (u32, u32), fn(u32, u32) -> (u32, u32));

#[test]
fn registers_with_documented_defaults() {
    init();
    let element = gst::ElementFactory::make("wallwarp")
        .build()
        .expect("wallwarp");
    assert_eq!(element.property::<String>("quad"), "0,0,1,0,1,1,0,1");
    assert_eq!(element.property::<u32>("output-width"), 0);
    assert_eq!(element.property::<u32>("output-height"), 0);
    assert_eq!(element.property::<u32>("background"), 0xff00_0000);
    assert_eq!(element.property::<u32>("n-threads"), 1);
    let interpolation = element.property_value("interpolation");
    let (_, value) = glib::EnumValue::from_value(&interpolation).expect("enum");
    assert_eq!(value.nick(), "bilinear");
}

#[test]
fn identity_passes_frames_through() {
    let mut h = harness("", (8, 6));
    let input = grid(8, 6, 0, 0);
    let rows = warp(&mut h, input);
    assert_samples(&rows, |x, y| Some((x, y)));
    let transform = element(&h)
        .downcast::<gst_base::BaseTransform>()
        .expect("base transform");
    assert!(transform.is_passthrough());
}

#[test]
fn identity_scales_to_another_output_size() {
    let mut h = harness(
        "interpolation=nearest output-width=8 output-height=6",
        (4, 3),
    );
    let rows = warp(&mut h, grid(4, 3, 0, 0));
    assert_eq!(size(&rows), (8, 6));
    assert_samples(&rows, |x, y| Some((x / 2, y / 2)));
}

#[test]
fn rectangles_crop_source_pixels() {
    for interpolation in INTERPOLATIONS {
        let mut h = harness(
            &format!(
                "quad=0.25,0.25,0.75,0.25,0.75,0.75,0.25,0.75 \
                 output-width=8 output-height=6 interpolation={interpolation}"
            ),
            (16, 12),
        );
        let rows = warp(&mut h, grid(16, 12, 0, 0));
        assert_eq!(size(&rows), (8, 6), "{interpolation}");
        assert_samples(&rows, |x, y| Some((x + 4, y + 3)));
    }
}

#[test]
fn quarter_turns_rotate_pixels() {
    // A 6x4 source shown on displays whose own top-left sits at the source's
    // top-right (90°), bottom-right (180°), and bottom-left (270°).
    let turns: [Turn; 3] = [
        ("1,0,1,1,0,1,0,0", (4, 6), |x, y| (5 - y, x)),
        ("1,1,0,1,0,0,1,0", (6, 4), |x, y| (5 - x, 3 - y)),
        ("0,1,0,0,1,0,1,1", (4, 6), |x, y| (y, 3 - x)),
    ];
    for interpolation in INTERPOLATIONS {
        for (quad, (width, height), expected) in turns {
            let mut h = harness(
                &format!(
                    "quad={quad} output-width={width} output-height={height} \
                     interpolation={interpolation}"
                ),
                (6, 4),
            );
            let rows = warp(&mut h, grid(6, 4, 0, 0));
            assert_eq!(size(&rows), (width as usize, height as usize));
            assert_samples(&rows, |x, y| Some(expected(x, y)));
        }
    }
}

#[test]
fn trapezoids_sample_projective_coordinates() {
    let mut h = harness(
        "quad=0.25,0,0.75,0,1,1,0,1 output-width=33 output-height=33 interpolation=nearest",
        (64, 64),
    );
    let rows = warp(&mut h, grid(64, 64, 0, 0));
    let sample = |x: usize, y: usize| decode(rows[y][x]).expect("inside the source");
    // A perspective map sends the output center to where the trapezoid's
    // diagonals cross, a third of the way down; an affine or bilinear warp
    // would sample the source center instead.
    assert_eq!(sample(16, 16), (32, 21));
    for x in 0..33 {
        assert_eq!(sample(x, 16).1, 21, "the middle row samples one source row");
        assert_eq!(
            sample(16, x).0,
            32,
            "the middle column samples one source column"
        );
    }
    for ((x, y), corner) in
        [(0, 0), (32, 0), (32, 32), (0, 32)]
            .into_iter()
            .zip([(16, 0), (48, 0), (63, 63), (0, 63)])
    {
        let (sx, sy) = sample(x, y);
        assert!(
            sx.abs_diff(corner.0) <= 1 && sy.abs_diff(corner.1) <= 1,
            "output corner ({x}, {y}) sampled ({sx}, {sy}), expected near {corner:?}"
        );
    }
}

#[test]
fn threads_share_frames_without_changing_them() {
    // A perspective quad reaching past the source, on odd sizes that leave
    // threads unequal bands.
    let properties = "quad=-0.1,0.05,0.9,-0.05,1.05,1.1,0.1,0.95 output-width=61 output-height=37";
    for interpolation in INTERPOLATIONS {
        let rows = [1, 4, 0].map(|threads| {
            let mut h = harness(
                &format!("{properties} interpolation={interpolation} n-threads={threads}"),
                (50, 40),
            );
            warp(&mut h, grid(50, 40, 8, 0))
        });
        assert_eq!(size(&rows[0]), (61, 37));
        assert!(rows[1] == rows[0], "{interpolation} on 4 threads");
        assert!(rows[2] == rows[0], "{interpolation} on a thread per core");
    }
}

#[test]
fn samples_outside_the_source_use_the_background() {
    for interpolation in INTERPOLATIONS {
        let mut h = harness(
            &format!(
                "quad=-0.5,-0.5,1.5,-0.5,1.5,1.5,-0.5,1.5 output-width=8 output-height=8 \
                 background=0x80112233 interpolation={interpolation}"
            ),
            (4, 4),
        );
        let rows = warp(&mut h, grid(4, 4, 0, 0));
        for (y, row) in (0..).zip(&rows) {
            for (x, pixel) in (0..).zip(row) {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                let expected = if inside {
                    encode(x - 2, y - 2)
                } else {
                    [0x11, 0x22, 0x33, 0x80]
                };
                assert_eq!(*pixel, expected, "{interpolation} ({x}, {y})");
            }
        }
    }
}

#[test]
fn invalid_quads_fail_at_startup() {
    init();
    for quad in [
        "",
        "0,0,1,0,1,1,0",
        "0,0,1,0,1,1,0,1,0",
        "0,0,1,0,1,1,0,one",
        "nan,0,1,0,1,1,0,1",
        "0,0,1,1,1,0,0,1",
        "0,0,1,0,0.2,0.2,0,1",
        "0,0,0.5,0,1,0,0,1",
        "0,0,0,0,1,1,0,1",
    ] {
        let pipeline = gst::Pipeline::new();
        let element = gst::ElementFactory::make("wallwarp")
            .property("quad", quad)
            .build()
            .expect("wallwarp");
        pipeline.add(&element).expect("adding wallwarp");
        assert_eq!(element.property::<String>("quad"), quad);
        let _failure = element
            .set_state(gst::State::Paused)
            .expect_err("an invalid quad refuses to start");
        let message = pipeline
            .bus()
            .expect("bus")
            .timed_pop_filtered(gst::ClockTime::from_seconds(1), &[gst::MessageType::Error])
            .expect("error message");
        let gst::MessageView::Error(error) = message.view() else {
            panic!("expected an error message");
        };
        assert!(
            error.error().matches(gst::LibraryError::Settings)
                && error.error().message().contains("invalid quad"),
            "{quad:?}: {error:?}"
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("stopping the pipeline");
    }
}

#[test]
fn sizes_and_sampling_are_fixed_while_streaming() {
    let mut h = harness("output-width=4 output-height=4", (8, 8));
    let before = warp(&mut h, grid(8, 8, 0, 0));
    let element = element(&h);
    element.set_property("output-width", 2u32);
    element.set_property_from_str("interpolation", "bilinear");
    assert_eq!(element.property::<u32>("output-width"), 4);
    assert_eq!(warp(&mut h, grid(8, 8, 0, 0)), before);
}

#[test]
fn quads_change_while_streaming() {
    for interpolation in INTERPOLATIONS {
        let mut h = harness(
            &format!("output-width=4 output-height=4 interpolation={interpolation}"),
            (8, 8),
        );
        assert_samples(&warp(&mut h, grid(8, 8, 0, 0)), |x, y| {
            Some((2 * x + 1, 2 * y + 1))
        });
        let element = element(&h);
        // The top-left quarter, at one output pixel per source pixel.
        element.set_property("quad", "0,0,0.5,0,0.5,0.5,0,0.5");
        assert_eq!(
            element.property::<String>("quad"),
            "0,0,0.5,0,0.5,0.5,0,0.5"
        );
        assert_samples(&warp(&mut h, grid(8, 8, 0, 0)), |x, y| Some((x, y)));
        // An invalid quad leaves the current one in place.
        element.set_property("quad", "0,0,1,1,1,0,0,1");
        assert_eq!(
            element.property::<String>("quad"),
            "0,0,0.5,0,0.5,0.5,0,0.5"
        );
        assert_samples(&warp(&mut h, grid(8, 8, 0, 0)), |x, y| Some((x, y)));
        // Back to the whole source.
        element.set_property("quad", "0,0,1,0,1,1,0,1");
        assert_samples(&warp(&mut h, grid(8, 8, 0, 0)), |x, y| {
            Some((2 * x + 1, 2 * y + 1))
        });
    }
}

#[test]
fn padded_input_rows_and_odd_sizes_are_supported() {
    // 7x5 input rows padded by 12 bytes after an 8-byte offset.
    let mut h = harness(
        "interpolation=nearest output-width=14 output-height=10",
        (7, 5),
    );
    let rows = warp(&mut h, grid(7, 5, 12, 8));
    assert_eq!(size(&rows), (14, 10));
    assert_samples(&rows, |x, y| Some((x / 2, y / 2)));

    for interpolation in INTERPOLATIONS {
        let mut h = harness(
            &format!("quad=1,1,0,1,0,0,1,0 interpolation={interpolation}"),
            (7, 5),
        );
        let rows = warp(&mut h, grid(7, 5, 12, 8));
        assert_eq!(size(&rows), (7, 5));
        assert_samples(&rows, |x, y| Some((6 - x, 4 - y)));
    }
}

#[test]
fn output_size_follows_properties_then_downstream_then_input() {
    let quad = "quad=0.1,0,1,0,0.9,1,0,1";
    let cases = [
        (
            format!("{quad} output-width=10 output-height=4"),
            None,
            (10, 4),
        ),
        (
            quad.to_owned(),
            Some("video/x-raw,format=RGBA,width=5,height=3"),
            (5, 3),
        ),
        (
            format!("{quad} output-width=3"),
            Some("video/x-raw,format=RGBA,width=[1,100],height=[2,100]"),
            (3, 6),
        ),
        (quad.to_owned(), None, (8, 6)),
    ];
    for (properties, downstream, (width, height)) in cases {
        let mut h = harness(&properties, (8, 6));
        if let Some(downstream) = downstream {
            h.set_sink_caps_str(downstream);
        }
        let rows = warp(&mut h, grid(8, 6, 0, 0));
        assert_eq!(size(&rows), (width, height), "{properties} {downstream:?}");
        let caps = output_caps(&h);
        let structure = caps.structure(0).expect("structure");
        assert_eq!(
            structure.get::<gst::Fraction>("framerate"),
            Ok(gst::Fraction::new(30, 1))
        );
        assert_eq!(
            structure.get::<gst::Fraction>("pixel-aspect-ratio"),
            Ok(gst::Fraction::new(1, 1))
        );
    }
}

#[test]
fn source_pixel_aspect_ratio_does_not_reach_the_output() {
    init();
    let mut h = gst_check::Harness::new_parse("wallwarp output-width=4 output-height=4");
    h.set_src_caps_str(
        "video/x-raw,format=RGBA,width=8,height=8,framerate=30/1,pixel-aspect-ratio=2/1",
    );
    h.push(grid(8, 8, 0, 0)).expect("pushing a frame");
    let caps = output_caps(&h);
    assert_eq!(
        caps.structure(0)
            .expect("structure")
            .get::<gst::Fraction>("pixel-aspect-ratio"),
        Ok(gst::Fraction::new(1, 1))
    );
}

#[test]
fn incompatible_caps_fail_negotiation() {
    let mut h = harness("output-width=10 output-height=4", (8, 6));
    h.set_sink_caps_str("video/x-raw,format=RGBA,width=5,height=3");
    assert_eq!(h.push(grid(8, 6, 0, 0)), Err(gst::FlowError::NotNegotiated));

    init();
    let element = gst::ElementFactory::make("wallwarp")
        .build()
        .expect("wallwarp");
    let sink = element.static_pad("sink").expect("sink pad");
    assert!(sink.query_accept_caps(&caps(8, 6)));
    assert!(
        !sink.query_accept_caps(
            &"video/x-raw,format=I420,width=8,height=6"
                .parse::<gst::Caps>()
                .expect("caps")
        )
    );
}

#[test]
fn timestamps_flags_and_events_are_preserved() {
    let mut h = harness(
        "interpolation=nearest output-width=4 output-height=2",
        (8, 4),
    );
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_start(gst::ClockTime::from_seconds(1));
    segment.set_base(gst::ClockTime::from_mseconds(250));
    segment.set_rate(2.0);
    assert!(h.push_event(gst::event::Segment::new(&segment)));

    let flags = gst::BufferFlags::DISCONT
        | gst::BufferFlags::DELTA_UNIT
        | gst::BufferFlags::MARKER
        | gst::BufferFlags::HEADER;
    let mut input = grid(8, 4, 0, 0);
    {
        let input = input.get_mut().expect("writable");
        input.set_pts(gst::ClockTime::from_mseconds(1_010));
        input.set_dts(gst::ClockTime::from_mseconds(1_000));
        input.set_duration(gst::ClockTime::from_mseconds(33));
        input.set_offset(5);
        input.set_offset_end(6);
        input.set_flags(flags);
    }
    h.push(input).expect("pushing a frame");
    let output = h.pull().expect("output frame");
    assert_eq!(output.pts(), Some(gst::ClockTime::from_mseconds(1_010)));
    assert_eq!(output.dts(), Some(gst::ClockTime::from_mseconds(1_000)));
    assert_eq!(output.duration(), Some(gst::ClockTime::from_mseconds(33)));
    assert_eq!((output.offset(), output.offset_end()), (5, 6));
    assert!(output.flags().contains(flags), "{:?}", output.flags());

    let forwarded = drain_events(&mut h);
    let last_segment = forwarded
        .iter()
        .rev()
        .find_map(|event| match event.view() {
            gst::EventView::Segment(event) => Some(event.segment().clone()),
            _ => None,
        })
        .expect("segment event");
    assert_eq!(
        last_segment.downcast_ref::<gst::ClockTime>(),
        Some(&segment)
    );

    assert!(h.push_event(gst::event::FlushStart::new()));
    assert!(h.push_event(gst::event::FlushStop::new(true)));
    let kinds = drain_events(&mut h)
        .iter()
        .map(|event| event.type_())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [gst::EventType::FlushStart, gst::EventType::FlushStop]
    );

    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    let rows = warp(&mut h, grid(8, 4, 0, 0));
    assert_samples(&rows, |x, y| Some((2 * x + 1, 2 * y + 1)));

    assert!(h.push_event(gst::event::Eos::new()));
    assert!(
        drain_events(&mut h)
            .iter()
            .any(|event| event.type_() == gst::EventType::Eos)
    );
}

/// Whether `point` lies within `margin` of a convex, clockwise quadrilateral,
/// tested with edge half-planes rather than a homography.
fn inside(quad: &Quad, point: Point, margin: f64) -> bool {
    let corners = quad.corners();
    corners
        .iter()
        .zip(corners.iter().cycle().skip(1))
        .all(|(a, b)| {
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            dx * (point.y - a.y) - dy * (point.x - a.x) >= -margin * dx.hypot(dy)
        })
}

fn check_screen(layout: &Layout, screen: &Screen, rows: &Rows) {
    let id = &screen.id;
    let (width, height) = (screen.output.width, screen.output.height);
    assert_eq!(size(rows), (width as usize, height as usize), "{id}");
    let local_to_source = layout.local_to_source(screen).expect("mapping");
    let (canvas_width, canvas_height) = (
        f64::from(layout.canvas.width),
        f64::from(layout.canvas.height),
    );
    let sampled_row = |y: u32| y.is_multiple_of(7) || y + 1 == height;
    let sampled_column = |x: u32| x.is_multiple_of(7) || x + 1 == width;
    for (y, row) in (0..).zip(rows).filter(|(y, _)| sampled_row(*y)) {
        for (x, pixel) in (0..).zip(row).filter(|(x, _)| sampled_column(*x)) {
            let sample = decode(*pixel);
            assert!(
                sample.is_some(),
                "{id} ({x}, {y}) sampled outside the source"
            );
            let (sx, sy) = sample.expect("asserted above");
            let (sx, sy) = (f64::from(sx), f64::from(sy));
            // Gaps between screens never appear: every sample is on this screen.
            assert!(
                inside(&screen.corners, Point::new(sx + 0.5, sy + 0.5), 1.0),
                "{id} ({x}, {y}) sampled ({sx}, {sy}) off the screen"
            );
            // The sample is the source pixel containing the mapped pixel center.
            let local = Point::new(
                (f64::from(x) + 0.5) / f64::from(width),
                (f64::from(y) + 0.5) / f64::from(height),
            );
            let mapped = local_to_source.map(local).expect("finite mapping");
            let (mx, my) = (mapped.x * canvas_width, mapped.y * canvas_height);
            assert!(
                (sx - 1e-6..sx + 1.0 + 1e-6).contains(&mx)
                    && (sy - 1e-6..sy + 1.0 + 1e-6).contains(&my),
                "{id} ({x}, {y}) sampled ({sx}, {sy}) instead of ({mx}, {my})"
            );
        }
    }
    // Output corners show the screen corners in the display's own orientation.
    let output_corners = [
        (0, 0),
        (width - 1, 0),
        (width - 1, height - 1),
        (0, height - 1),
    ];
    for ((x, y), corner) in output_corners.into_iter().zip(screen.corners.corners()) {
        let pixel = rows
            .get(y as usize)
            .and_then(|row| row.get(x as usize))
            .expect("corner pixel");
        let (sx, sy) = decode(*pixel).expect("corner sample");
        assert!(
            (f64::from(sx) + 0.5 - corner.x).abs() < 2.0
                && (f64::from(sy) + 0.5 - corner.y).abs() < 2.0,
            "{id} corner ({x}, {y}) sampled ({sx}, {sy}), expected near {corner:?}"
        );
    }
}

#[test]
fn three_screen_layout_feeds_independent_branches() {
    init();
    let layout = Layout::from_json(LAYOUT).expect("bundled layout");
    let (width, height) = (layout.canvas.width, layout.canvas.height);
    layout
        .check_source_size(width, height)
        .expect("source matches the canvas");

    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("appsrc")
        .property("caps", caps(width, height))
        .property("format", gst::Format::Time)
        .build()
        .expect("appsrc");
    let tee = gst::ElementFactory::make("tee").build().expect("tee");
    pipeline.add_many([&source, &tee]).expect("adding source");
    source.link(&tee).expect("linking source");
    let branches = layout
        .screens
        .iter()
        .map(|screen| {
            let queue = gst::ElementFactory::make("queue").build().expect("queue");
            let warp = gst::ElementFactory::make("wallwarp")
                .name(screen.id.as_str())
                .property("quad", layout.source_quad(screen).to_string())
                .property("output-width", screen.output.width)
                .property("output-height", screen.output.height)
                .property("background", 0u32)
                .property_from_str("interpolation", "nearest")
                .build()
                .expect("wallwarp");
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                .expect("fakesink");
            pipeline
                .add_many([&queue, &warp, &sink])
                .expect("adding branch");
            gst::Element::link_many([&tee, &queue, &warp, &sink]).expect("linking branch");
            (screen, sink)
        })
        .collect::<Vec<_>>();

    pipeline
        .set_state(gst::State::Playing)
        .expect("starting the pipeline");
    let mut frame = grid(width, height, 0, 0);
    frame
        .get_mut()
        .expect("writable")
        .set_pts(gst::ClockTime::ZERO);
    assert_eq!(
        source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&frame]),
        gst::FlowReturn::Ok
    );
    assert_eq!(
        source.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]),
        gst::FlowReturn::Ok
    );
    let message = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    assert!(
        matches!(message.view(), gst::MessageView::Eos(_)),
        "{message:?}"
    );

    for (screen, sink) in branches {
        let sample = sink
            .property::<Option<gst::Sample>>("last-sample")
            .expect("rendered frame");
        let rows = pixels(
            sample.buffer().expect("buffer"),
            sample.caps().expect("caps"),
        );
        check_screen(&layout, screen, &rows);
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("stopping the pipeline");
}
