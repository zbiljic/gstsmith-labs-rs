#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

//! `walltiles` gives each display of a layout the frames `wallwarp` makes
//! for it.

use gst::glib;
use gst::prelude::*;
use gst_video::prelude::*;
use wall_geometry::Layout;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstwallwarp::plugin_register_static().expect("registering the wallwarp plugin");
    });
}

/// A landscape display on the canvas's top-left quarter and one mounted a
/// quarter turn clockwise on its bottom-right quarter.
const LAYOUT: &str = r#"{
  "version": 1,
  "canvas": {"width": 64, "height": 48},
  "screens": [
    {"id": "left", "output": {"width": 32, "height": 24},
     "corners": [[0, 0], [32, 0], [32, 24], [0, 24]]},
    {"id": "turned", "output": {"width": 24, "height": 32},
     "corners": [[64, 24], [64, 48], [32, 48], [32, 24]]}
  ]
}"#;

/// The same displays, the left one now on the whole canvas at another size,
/// with a mask.
const MOVED: &str = r#"{
  "version": 1,
  "canvas": {"width": 64, "height": 48},
  "screens": [
    {"id": "left", "output": {"width": 40, "height": 30},
     "corners": [[0, 0], [64, 0], [64, 48], [0, 48]],
     "mask": [[[0.5, 0], [1, 1], [0, 1]]]},
    {"id": "turned", "output": {"width": 24, "height": 32},
     "corners": [[64, 24], [64, 48], [32, 48], [32, 24]]}
  ]
}"#;

/// A frame whose pixels encode their coordinates.
fn grid(width: u32, height: u32, index: u64) -> gst::Buffer {
    let data = (0..height)
        .flat_map(|y| {
            (0..width).flat_map(move |x| {
                let [x_low, x_high, ..] = x.to_le_bytes();
                let [y_low, y_high, ..] = y.to_le_bytes();
                [x_low, y_low, x_high | (y_high << 4), 0xff]
            })
        })
        .collect::<Vec<_>>();
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer = buffer.get_mut().expect("writable");
        let duration = gst::ClockTime::from_nseconds(33_333_333);
        buffer.set_pts(duration * index);
        buffer.set_duration(duration);
    }
    buffer
}

fn appsrc(width: u32, height: u32) -> gst::Element {
    let caps = gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(i32::try_from(width).expect("width"))
        .height(i32::try_from(height).expect("height"))
        .framerate(gst::Fraction::new(30, 1))
        .build();
    gst::ElementFactory::make("appsrc")
        .property("caps", caps)
        .property_from_str("format", "time")
        .build()
        .expect("appsrc")
}

fn appsink() -> gst::Element {
    gst::ElementFactory::make("appsink")
        .property("sync", false)
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .build()
        .expect("appsink")
}

fn push(source: &gst::Element, buffer: &gst::Buffer) {
    let pushed = source.emit_by_name::<gst::FlowReturn>("push-buffer", &[buffer]);
    assert_eq!(pushed, gst::FlowReturn::Ok, "pushing a frame");
}

/// An output frame's size and pixels, row after row.
type Frame = ((u32, u32), Vec<[u8; 4]>);

fn pull(sink: &gst::Element) -> Frame {
    let sample = sink
        .emit_by_name::<Option<gst::Sample>>(
            "try-pull-sample",
            &[&gst::ClockTime::from_seconds(10)],
        )
        .expect("an output frame");
    let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
    let buffer = sample.buffer().expect("buffer");
    let frame =
        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("readable frame");
    let stride =
        usize::try_from(*frame.plane_stride().first().expect("stride")).expect("positive stride");
    let row = info.width() as usize * 4;
    let pixels = frame
        .plane_data(0)
        .expect("plane")
        .chunks(stride)
        .take(info.height() as usize)
        .flat_map(|data| data.get(..row).expect("row").as_chunks::<4>().0.to_vec())
        .collect();
    ((info.width(), info.height()), pixels)
}

/// What `wallwarp` alone makes of a frame for `id` in `layout`.
fn reference(layout: &str, id: &str, buffer: &gst::Buffer) -> Frame {
    let layout = Layout::from_json(layout).expect("layout");
    let screen = layout.screen(id).expect("screen");
    let pipeline = gst::Pipeline::new();
    let source = appsrc(layout.canvas.width, layout.canvas.height);
    let warp = gst::ElementFactory::make("wallwarp")
        .property("quad", layout.source_quad(screen).to_string())
        .property("mask", screen.mask.to_string())
        .property_from_str("interpolation", "nearest")
        .property("output-width", screen.output.width)
        .property("output-height", screen.output.height)
        .build()
        .expect("wallwarp");
    let sink = appsink();
    pipeline.add_many([&source, &warp, &sink]).expect("adding");
    gst::Element::link_many([&source, &warp, &sink]).expect("linking");
    pipeline.set_state(gst::State::Playing).expect("playing");
    push(&source, buffer);
    let frame = pull(&sink);
    pipeline.set_state(gst::State::Null).expect("stopping");
    frame
}

/// `appsrc ! walltiles` with an appsink on each of `ids`' pads.
struct Wall {
    pipeline: gst::Pipeline,
    source: gst::Element,
    tiles: gst::Element,
    sinks: Vec<(String, gst::Element)>,
    frames: u64,
}

impl Wall {
    fn new(properties: &[(&str, glib::Value)], ids: &[&str]) -> Self {
        init();
        let pipeline = gst::Pipeline::new();
        let source = appsrc(64, 48);
        let mut tiles = gst::ElementFactory::make("walltiles")
            .property("layout", LAYOUT)
            .property_from_str("interpolation", "nearest");
        for (name, value) in properties {
            tiles = tiles.property(name, value.clone());
        }
        let tiles = tiles.build().expect("walltiles");
        pipeline.add_many([&source, &tiles]).expect("adding");
        source.link(&tiles).expect("linking the source");
        let mut wall = Self {
            pipeline,
            source,
            tiles,
            sinks: Vec::new(),
            frames: 0,
        };
        for id in ids {
            wall.add(id);
        }
        wall.pipeline
            .set_state(gst::State::Playing)
            .expect("playing");
        wall
    }

    /// Request `id`'s pad and play it into a new appsink.
    fn add(&mut self, id: &str) {
        let pad = self
            .tiles
            .request_pad_simple(&format!("src_{id}"))
            .expect("display pad");
        let sink = appsink();
        self.pipeline.add(&sink).expect("adding the sink");
        pad.link(&sink.static_pad("sink").expect("sink pad"))
            .expect("linking the display");
        sink.sync_state_with_parent().expect("starting the sink");
        self.sinks.push((id.to_owned(), sink));
    }

    /// Push the next frame and return it with what each display got.
    fn frame(&mut self) -> (gst::Buffer, Vec<(String, Frame)>) {
        let buffer = grid(64, 48, self.frames);
        self.frames += 1;
        push(&self.source, &buffer);
        let frames = self
            .sinks
            .iter()
            .map(|(id, sink)| (id.clone(), pull(sink)))
            .collect();
        (buffer, frames)
    }

    /// Assert the next frame of every display is what `wallwarp` makes of
    /// it with `layout`.
    fn assert_frames(&mut self, layout: &str) {
        let (buffer, frames) = self.frame();
        for (id, frame) in frames {
            assert_eq!(frame, reference(layout, &id, &buffer), "display {id}");
        }
    }
}

impl Drop for Wall {
    fn drop(&mut self) {
        let _ignored = self.pipeline.set_state(gst::State::Null);
    }
}

#[test]
fn every_display_gets_its_part_of_the_frame() {
    let mut wall = Wall::new(&[], &["left", "turned"]);
    wall.assert_frames(LAYOUT);
    let (_, frames) = wall.frame();
    let sizes: Vec<_> = frames
        .iter()
        .map(|(id, ((w, h), _))| (id.as_str(), *w, *h))
        .collect();
    assert_eq!(sizes, [("left", 32, 24), ("turned", 24, 32)]);
}

#[test]
fn layouts_change_while_playing() {
    let mut wall = Wall::new(&[], &["left", "turned"]);
    wall.assert_frames(LAYOUT);
    // Moved, resized and masked at once.
    wall.tiles.set_property("layout", MOVED);
    wall.assert_frames(MOVED);
    let (_, frames) = wall.frame();
    assert_eq!(
        frames.first().map(|(_, ((w, h), _))| (*w, *h)),
        Some((40, 30))
    );
    // An invalid layout is ignored.
    wall.tiles.set_property("layout", "{\"version\": 1}");
    wall.assert_frames(MOVED);
    // Settings shared by the displays reach every warp.
    wall.tiles.set_property("background", 0xff10_2030_u32);
    wall.tiles
        .set_property_from_str("interpolation", "bilinear");
    for id in ["left", "turned"] {
        let warp = wall
            .tiles
            .downcast_ref::<gst::Bin>()
            .expect("bin")
            .by_name(&format!("warp-{id}"))
            .expect("warp");
        assert_eq!(warp.property::<u32>("background"), 0xff10_2030);
        let interpolation = warp.property_value("interpolation");
        let (_, value) = glib::EnumValue::from_value(&interpolation).expect("enum");
        assert_eq!(value.nick(), "bilinear");
    }
}

#[test]
fn layouts_come_from_files() {
    init();
    let path = std::env::temp_dir().join(format!("walltiles-{}.json", std::process::id()));
    std::fs::write(&path, LAYOUT).expect("writing the layout");
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers=3 ! video/x-raw,width=64,height=48 \
         ! walltiles name=tiles location=\"{}\" \
         tiles.src_left ! fakesink name=left \
         tiles.src_turned ! fakesink name=turned",
        path.display()
    ))
    .expect("pipeline")
    .downcast::<gst::Pipeline>()
    .expect("a pipeline");
    pipeline.set_state(gst::State::Playing).expect("playing");
    let message = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("a message");
    assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
    let size = |name: &str| {
        let caps = pipeline
            .by_name(name)
            .expect("sink")
            .static_pad("sink")
            .expect("pad")
            .current_caps()
            .expect("caps");
        let info = gst_video::VideoInfo::from_caps(&caps).expect("info");
        (info.width(), info.height())
    };
    assert_eq!(size("left"), (32, 24));
    assert_eq!(size("turned"), (24, 32));
    pipeline.set_state(gst::State::Null).expect("stopping");
    let tiles = pipeline.by_name("tiles").expect("walltiles");
    assert_eq!(
        tiles.property::<Option<String>>("location").as_deref(),
        path.to_str()
    );
    assert_eq!(
        Layout::from_json(&tiles.property::<String>("layout")).expect("layout"),
        Layout::from_json(LAYOUT).expect("layout")
    );
    std::fs::remove_file(&path).expect("removing the layout");
}

#[test]
fn pads_are_refused_for_displays_outside_the_layout() {
    init();
    let tiles = gst::ElementFactory::make("walltiles")
        .build()
        .expect("walltiles");
    // The warps' own defaults.
    assert_eq!(tiles.property::<u32>("background"), 0xff00_0000);
    assert!(!tiles.property::<bool>("gl"));
    assert_eq!(tiles.property::<Option<String>>("layout"), None);
    assert!(
        tiles.request_pad_simple("src_left").is_none(),
        "no layout yet"
    );
    tiles.set_property("layout", LAYOUT);
    assert!(tiles.request_pad_simple("src_middle").is_none());
    let pad = tiles.request_pad_simple("src_left").expect("left");
    assert_eq!(pad.name(), "src_left");
    assert!(tiles.request_pad_simple("src_left").is_none(), "twice");
    tiles.release_request_pad(&pad);
    assert!(
        tiles.request_pad_simple("src_left").is_some(),
        "after release"
    );
}

#[test]
fn displays_come_and_go_while_playing() {
    displays_come_and_go(false);
}

#[test]
fn displays_come_and_go_on_the_gpu() {
    if gl() {
        displays_come_and_go(true);
    }
}

fn displays_come_and_go(gl: bool) {
    let mut wall = Wall::new(&[("gl", gl.to_value())], &["left"]);
    wall.assert_frames(LAYOUT);
    wall.add("turned");
    wall.assert_frames(LAYOUT);
    let bin = wall.tiles.clone().downcast::<gst::Bin>().expect("bin");
    for round in 0..5 {
        // The left display goes, at once; the turned one plays on.
        let (_, sink) = wall.sinks.remove(0);
        sink.set_state(gst::State::Null).expect("stopping the sink");
        let pad = wall.tiles.static_pad("src_left").expect("left pad");
        wall.tiles.release_request_pad(&pad);
        wall.pipeline.remove(&sink).expect("removing the sink");
        for name in ["queue-left", "warp-left", "size-left"] {
            assert!(bin.by_name(name).is_none(), "{name} stayed, round {round}");
        }
        wall.assert_frames(LAYOUT);
        // And comes back.
        wall.add("left");
        wall.sinks.rotate_right(1);
        wall.assert_frames(LAYOUT);
    }
}

#[test]
fn released_displays_can_be_requested_again_at_once() {
    init();
    let tiles = gst::ElementFactory::make("walltiles")
        .property("layout", LAYOUT)
        .build()
        .expect("walltiles");
    for _ in 0..100 {
        let pad = tiles.request_pad_simple("src_left").expect("left");
        tiles.release_request_pad(&pad);
    }
    let bin = tiles.downcast_ref::<gst::Bin>().expect("bin");
    // Only the conversion and the tee are left.
    assert_eq!(bin.children().len(), 2, "{:?}", bin.children());
}

#[test]
fn handlers_may_call_back_while_a_pad_is_added() {
    init();
    let tiles = gst::ElementFactory::make("walltiles")
        .property("layout", LAYOUT)
        .build()
        .expect("walltiles");
    // A pad-added handler that changes the element and asks for another
    // display, as an application might.
    tiles.connect_pad_added(|tiles, pad| {
        tiles.set_property("background", 0xff20_2020_u32);
        tiles.set_property("layout", LAYOUT);
        if pad.name() == "src_left" {
            assert!(tiles.request_pad_simple("src_turned").is_some());
        }
    });
    let (sender, receiver) = std::sync::mpsc::channel();
    let requester = tiles.clone();
    std::thread::spawn(move || {
        let pad = requester.request_pad_simple("src_left");
        let _ignored = sender.send(pad.is_some());
    });
    assert_eq!(
        receiver.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(true),
        "the request returns"
    );
    assert!(tiles.static_pad("src_turned").is_some());
}

#[test]
fn pads_requested_while_playing_convert_the_source() {
    init();
    // An I420 live source playing with no display yet.
    let pipeline = gst::parse::launch(
        "videotestsrc is-live=true \
         ! video/x-raw,format=I420,width=64,height=48,framerate=30/1 \
         ! walltiles name=tiles interpolation=nearest",
    )
    .expect("pipeline")
    .downcast::<gst::Pipeline>()
    .expect("a pipeline");
    pipeline
        .by_name("tiles")
        .expect("walltiles")
        .set_property("layout", LAYOUT);
    pipeline.set_state(gst::State::Playing).expect("playing");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let tiles = pipeline.by_name("tiles").expect("walltiles");
    let pad = tiles.request_pad_simple("src_turned").expect("turned");
    let sink = appsink();
    pipeline.add(&sink).expect("adding");
    pad.link(&sink.static_pad("sink").expect("sink pad"))
        .expect("linking");
    sink.sync_state_with_parent().expect("starting");
    for _ in 0..3 {
        let ((width, height), _) = pull(&sink);
        assert_eq!((width, height), (24, 32));
    }
    pipeline.set_state(gst::State::Null).expect("stopping");
}

#[test]
fn sources_of_another_shape_are_warned_about() {
    init();
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 1)
        .build()
        .expect("videotestsrc");
    // Square, on a 4:3 canvas.
    let square = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 64)
                .field("height", 64)
                .build(),
        )
        .build()
        .expect("capsfilter");
    let tiles = gst::ElementFactory::make("walltiles")
        .property("layout", LAYOUT)
        .build()
        .expect("walltiles");
    let sink = gst::ElementFactory::make("fakesink").build().expect("sink");
    pipeline
        .add_many([&source, &square, &tiles, &sink])
        .expect("adding");
    gst::Element::link_many([&source, &square, &tiles]).expect("linking");
    tiles
        .request_pad_simple("src_left")
        .expect("left")
        .link(&sink.static_pad("sink").expect("sink pad"))
        .expect("linking the display");
    pipeline.set_state(gst::State::Playing).expect("playing");
    let message = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Warning, gst::MessageType::Eos],
        )
        .expect("a message");
    assert_eq!(message.type_(), gst::MessageType::Warning, "{message:?}");
    pipeline.set_state(gst::State::Null).expect("stopping");
}

/// Whether OpenGL works here, saying so when it does not.
fn gl() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    init();
    *WORKS.get_or_init(|| {
        let works = gst::parse::launch("gltestsrc num-buffers=1 ! gldownload ! fakesink")
            .ok()
            .and_then(|pipeline| pipeline.downcast::<gst::Pipeline>().ok())
            .is_some_and(|pipeline| {
                let done = pipeline.set_state(gst::State::Playing).is_ok()
                    && pipeline
                        .bus()
                        .and_then(|bus| {
                            bus.timed_pop_filtered(
                                gst::ClockTime::from_seconds(10),
                                &[gst::MessageType::Eos, gst::MessageType::Error],
                            )
                        })
                        .is_some_and(|message| message.type_() == gst::MessageType::Eos);
                let _ignored = pipeline.set_state(gst::State::Null);
                done
            });
        if !works {
            eprintln!("skipping: no OpenGL context");
        }
        works
    })
}

#[test]
fn displays_warp_on_the_gpu_alike() {
    if !gl() {
        return;
    }
    let mut wall = Wall::new(&[("gl", true.to_value())], &["left", "turned"]);
    // Whole source pixels on whole output pixels: nothing to round.
    wall.assert_frames(LAYOUT);
    let bin = wall.tiles.downcast_ref::<gst::Bin>().expect("bin");
    let warp = bin.by_name("warp-left").expect("warp");
    assert_eq!(warp.factory().expect("factory").name(), "glwallwarp");
    // Too late to change.
    wall.tiles.set_property("gl", false);
    assert!(wall.tiles.property::<bool>("gl"));
    wall.tiles.set_property("layout", MOVED);
    wall.assert_frames(MOVED);
}
