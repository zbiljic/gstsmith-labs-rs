#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful GStreamer operations"
)]

//! `glwallwarp` draws what `wallwarp` computes. Tests skip where no OpenGL
//! context can be made.

use gst::glib;
use gst::prelude::*;
use gst_video::prelude::*;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstwallwarp::plugin_register_static().expect("registering the wallwarp plugin");
    });
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

/// An opaque color encoding pixel coordinates below 4096.
fn encode(x: u32, y: u32) -> [u8; 4] {
    let [x_low, x_high, ..] = x.to_le_bytes();
    let [y_low, y_high, ..] = y.to_le_bytes();
    [x_low, y_low, x_high | (y_high << 4), 0xff]
}

/// A frame whose pixels encode their coordinates.
fn grid(width: u32, height: u32) -> gst::Buffer {
    let data = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| encode(x, y)))
        .collect::<Vec<_>>();
    gst::Buffer::from_mut_slice(data)
}

/// A pipeline pushing frames through `element` with `properties` into an
/// appsink taking `output` caps.
struct Warp {
    pipeline: gst::Pipeline,
    source: gst::Element,
    sink: gst::Element,
    input: (u32, u32),
    frames: u64,
}

impl Warp {
    fn new(element: &str, properties: &str, input: (u32, u32), output: &str) -> Self {
        init();
        let (width, height) = input;
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=source format=time \
               caps=video/x-raw,format=RGBA,width={width},height={height},framerate=30/1 \
             ! {element} name=warp {properties} ! {output} \
             ! appsink name=sink sync=false"
        ))
        .expect("pipeline")
        .downcast::<gst::Pipeline>()
        .expect("a pipeline");
        let source = pipeline.by_name("source").expect("appsrc");
        let sink = pipeline.by_name("sink").expect("appsink");
        pipeline.set_state(gst::State::Playing).expect("playing");
        Self {
            pipeline,
            source,
            sink,
            input,
            frames: 0,
        }
    }

    fn element(&self) -> gst::Element {
        self.pipeline.by_name("warp").expect("warp element")
    }

    /// Push a frame and return the output's size and its rows of pixels,
    /// for RGBA output.
    fn frame(&mut self) -> Frame {
        rows(&self.sample())
    }

    /// Push a frame and return what came out.
    fn sample(&mut self) -> gst::Sample {
        push(&self.source, self.input, self.frames);
        self.frames += 1;
        pull(&self.sink)
    }
}

/// An output frame's size and rows of RGBA pixels.
type Frame = ((u32, u32), Vec<Vec<[u8; 4]>>);

/// Push the `index`th frame of a coordinate grid of `size` into `source`.
fn push(source: &gst::Element, size: (u32, u32), index: u64) {
    let mut buffer = grid(size.0, size.1);
    {
        let buffer = buffer.get_mut().expect("writable");
        let duration = gst::ClockTime::from_nseconds(33_333_333);
        buffer.set_pts(duration * index);
        buffer.set_duration(duration);
    }
    let pushed = source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]);
    assert_eq!(pushed, gst::FlowReturn::Ok, "pushing a frame");
}

fn pull(sink: &gst::Element) -> gst::Sample {
    sink.emit_by_name::<Option<gst::Sample>>(
        "try-pull-sample",
        &[&gst::ClockTime::from_seconds(10)],
    )
    .expect("an output frame")
}

fn rows(sample: &gst::Sample) -> Frame {
    let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
    let buffer = sample.buffer().expect("buffer");
    let frame =
        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("readable frame");
    let stride =
        usize::try_from(*frame.plane_stride().first().expect("stride")).expect("positive stride");
    let row = info.width() as usize * 4;
    let rows = frame
        .plane_data(0)
        .expect("plane")
        .chunks(stride)
        .take(info.height() as usize)
        .map(|data| data.get(..row).expect("row").as_chunks::<4>().0.to_vec())
        .collect();
    ((info.width(), info.height()), rows)
}

impl Drop for Warp {
    fn drop(&mut self) {
        let _ignored = self.pipeline.set_state(gst::State::Null);
    }
}

const RGBA: &str = "video/x-raw,format=RGBA";

/// The source pixel an output pixel shows, or `None` for `background`.
fn decode(pixel: [u8; 4], background: [u8; 4]) -> Option<(u32, u32)> {
    let [r, g, b, a] = pixel;
    (a == 0xff && pixel != background).then(|| {
        (
            u32::from(r) | (u32::from(b & 0x0f) << 8),
            u32::from(g) | (u32::from(b >> 4) << 8),
        )
    })
}

/// How the GPU may differ from the CPU.
#[derive(Clone, Copy)]
enum Match {
    /// The same source pixel, except where a sample lies just short of a
    /// boundary, which the GPU's nudge moves forward to the next pixel.
    Nearest,
    /// Blends within one level per channel: the GPU weighs them at its own
    /// precision.
    Bilinear,
}

/// Assert both elements give the same next frame.
fn assert_same(cpu: &mut Warp, gl: &mut Warp, expected: Match, what: &str) {
    let background = gl.element().property::<u32>("background");
    assert_frames(&cpu.frame(), &gl.frame(), expected, background, what);
}

/// Assert a GL frame matches the CPU's, whose `background` is big-endian
/// ARGB.
fn assert_frames(cpu: &Frame, gl: &Frame, expected: Match, background: u32, what: &str) {
    let ((cpu_size, cpu_rows), (gl_size, gl_rows)) = (cpu, gl);
    let [alpha, red, green, blue] = background.to_be_bytes();
    let background = [red, green, blue, alpha];
    let cpu_size = *cpu_size;
    assert_eq!(*gl_size, cpu_size, "{what}: output size");
    let mut neighbours = 0;
    let mut worst = 0;
    for (y, (cpu_row, gl_row)) in cpu_rows.iter().zip(gl_rows).enumerate() {
        for (x, (cpu_pixel, gl_pixel)) in cpu_row.iter().zip(gl_row).enumerate() {
            let fine = match expected {
                Match::Nearest => {
                    let next = |a: u32, b: u32| b == a || b == a + 1;
                    let near = match (
                        decode(*cpu_pixel, background),
                        decode(*gl_pixel, background),
                    ) {
                        (Some(a), Some(b)) => next(a.0, b.0) && next(a.1, b.1),
                        _ => false,
                    };
                    neighbours += usize::from(cpu_pixel != gl_pixel && near);
                    cpu_pixel == gl_pixel || near
                }
                Match::Bilinear => {
                    let difference = cpu_pixel
                        .iter()
                        .zip(gl_pixel)
                        .map(|(a, b)| a.abs_diff(*b))
                        .max()
                        .unwrap_or(0);
                    worst = worst.max(difference);
                    difference <= 1
                }
            };
            assert!(
                fine,
                "{what}: pixel ({x}, {y}) is {gl_pixel:?} in GL, {cpu_pixel:?} on the CPU"
            );
        }
    }
    let pixels = cpu_size.0 * cpu_size.1;
    assert!(
        neighbours * 100 <= pixels as usize,
        "{what}: {neighbours} of {pixels} pixels show a neighbour"
    );
    eprintln!("{what}: {neighbours} neighbours, blends at most {worst} apart");
}

/// Quads covering the source's inside, its edges, a quarter turn, space
/// outside it, and points behind the projection.
const QUADS: [&str; 5] = [
    "0.1,0.2,0.8,0.1,0.9,0.7,0.2,0.9",
    "0,0,1,0,1,1,0,1",
    "1,0,1,1,0,1,0,0",
    "-0.5,-0.25,1.5,-0.25,1.5,1.25,-0.5,1.25",
    "0.4,0.1,0.6,0.1,0.9,0.9,0.1,0.9",
];

#[test]
fn frames_match_wallwarp() {
    if !gl() {
        return;
    }
    for (interpolation, expected) in [("nearest", Match::Nearest), ("bilinear", Match::Bilinear)] {
        for quad in QUADS {
            for (input, output) in [
                ((64, 48), (40, 30)),
                ((33, 17), (50, 41)),
                ((64, 48), (64, 48)),
            ] {
                let properties = format!(
                    "quad={quad} interpolation={interpolation} background=0xff102030 \
                     output-width={} output-height={}",
                    output.0, output.1
                );
                let mut cpu = Warp::new("wallwarp", &properties, input, RGBA);
                let mut gl = Warp::new("glwallwarp", &properties, input, RGBA);
                assert_same(
                    &mut cpu,
                    &mut gl,
                    expected,
                    &format!("{interpolation} {quad} {input:?} to {output:?}"),
                );
            }
        }
    }
}

/// A diamond with a hole in it. Uneven coordinates keep its edges off pixel
/// centers, where single and double precision could decide differently.
const MASK: &str = "0.503,0.013 0.987,0.497 0.511,0.991 0.017,0.483; \
                    0.391,0.403 0.613,0.389 0.597,0.607 0.409,0.593";

#[test]
fn masks_match_wallwarp() {
    if !gl() {
        return;
    }
    for (interpolation, expected) in [("nearest", Match::Nearest), ("bilinear", Match::Bilinear)] {
        for quad in [QUADS[0], QUADS[1], QUADS[3]] {
            // The same size and the identity quad would otherwise pass
            // frames through.
            for (input, output) in [
                ((64, 48), (40, 30)),
                ((33, 17), (50, 41)),
                ((64, 48), (64, 48)),
            ] {
                let properties = format!(
                    "quad={quad} interpolation={interpolation} mask=\"{MASK}\" \
                     output-width={} output-height={}",
                    output.0, output.1
                );
                let mut cpu = Warp::new("wallwarp", &properties, input, RGBA);
                let mut gl = Warp::new("glwallwarp", &properties, input, RGBA);
                assert_eq!(gl.element().property::<String>("mask"), MASK);
                assert_same(
                    &mut cpu,
                    &mut gl,
                    expected,
                    &format!("masked {interpolation} {quad} {input:?} to {output:?}"),
                );
            }
        }
    }
}

#[test]
fn masks_decide_ties_and_restarts_like_wallwarp() {
    if !gl() {
        return;
    }
    // Edges through pixel centers: x = 0.5 at an odd width, and 0.25, 0.75
    // at a width and height of 2 mod 4. A center on an edge is to its right
    // and below it.
    for (mask, output) in [
        ("0,0 0.5,0 0.5,1 0,1", (41, 31)),
        ("0.25,0.25 0.75,0.25 0.75,0.75 0.25,0.75", (42, 30)),
    ] {
        let properties = format!(
            "interpolation=nearest mask=\"{mask}\" output-width={} output-height={}",
            output.0, output.1
        );
        let mut cpu = Warp::new("wallwarp", &properties, (64, 48), RGBA);
        let mut gl = Warp::new("glwallwarp", &properties, (64, 48), RGBA);
        assert_same(&mut cpu, &mut gl, Match::Nearest, &format!("{mask} ties"));
        // Started again, the shader is rebuilt with the mask.
        for warp in [&cpu, &gl] {
            warp.pipeline.set_state(gst::State::Null).expect("stopping");
            warp.pipeline
                .set_state(gst::State::Playing)
                .expect("playing again");
        }
        assert_same(
            &mut cpu,
            &mut gl,
            Match::Nearest,
            &format!("{mask} restarted"),
        );
    }
    // An invalid mask refuses to start, as an invalid quad does.
    let pipeline = gst::Pipeline::new();
    let element = gst::ElementFactory::make("glwallwarp")
        .property("mask", "0,0 1,0")
        .build()
        .expect("glwallwarp");
    pipeline.add(&element).expect("adding");
    let _failure = element
        .set_state(gst::State::Paused)
        .expect_err("an invalid mask refuses to start");
    let message = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Error])
        .expect("an error message");
    let gst::MessageView::Error(error) = message.view() else {
        panic!("expected an error message");
    };
    assert!(
        error.error().message().contains("invalid mask"),
        "{error:?}"
    );
    let _ignored = pipeline.set_state(gst::State::Null);
}

#[test]
fn large_sources_halved_pick_the_same_pixels() {
    if !gl() {
        return;
    }
    // Every sample lands exactly between two source pixels, where single
    // precision errs most.
    for quad in [QUADS[1], QUADS[2]] {
        let properties =
            format!("quad={quad} interpolation=nearest output-width=1920 output-height=1080");
        let mut cpu = Warp::new("wallwarp", &properties, (3840, 2160), RGBA);
        let mut gl = Warp::new("glwallwarp", &properties, (3840, 2160), RGBA);
        assert_same(
            &mut cpu,
            &mut gl,
            Match::Nearest,
            &format!("4K {quad} halved"),
        );
    }
}

#[test]
fn settings_change_between_frames() {
    if !gl() {
        return;
    }
    let start = "interpolation=nearest output-width=40 output-height=30";
    let mut cpu = Warp::new("wallwarp", start, (64, 48), RGBA);
    let mut gl = Warp::new("glwallwarp", start, (64, 48), RGBA);
    assert_same(&mut cpu, &mut gl, Match::Nearest, "at the start");
    let changes: [(&str, glib::Value, Match); 8] = [
        ("mask", MASK.to_value(), Match::Nearest),
        // An invalid mask is ignored.
        ("mask", "0,0 1,1".to_value(), Match::Nearest),
        ("quad", QUADS[0].to_value(), Match::Nearest),
        ("background", 0xff40_5060_u32.to_value(), Match::Nearest),
        ("quad", QUADS[3].to_value(), Match::Nearest),
        ("output-width", 24_u32.to_value(), Match::Nearest),
        ("interpolation", "bilinear".to_value(), Match::Bilinear),
        ("mask", "".to_value(), Match::Bilinear),
    ];
    for (name, value, expected) in changes {
        for warp in [&cpu, &gl] {
            let element = warp.element();
            if name == "interpolation" {
                element.set_property_from_str(name, value.get::<&str>().expect("nick"));
            } else {
                element.set_property_from_value(name, &value);
            }
        }
        assert_same(
            &mut cpu,
            &mut gl,
            expected,
            &format!("after setting {name}"),
        );
    }
}

#[test]
fn invalid_quads_fail_at_startup_and_are_ignored_while_playing() {
    if !gl() {
        return;
    }
    init();
    let element = gst::ElementFactory::make("glwallwarp")
        .property("quad", "0,0,1,0")
        .build()
        .expect("glwallwarp");
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("videotestsrc")
        .build()
        .expect("source");
    let sink = gst::ElementFactory::make("fakesink").build().expect("sink");
    pipeline
        .add_many([&source, &element, &sink])
        .expect("adding");
    gst::Element::link_many([&source, &element, &sink]).expect("linking");
    assert_eq!(
        pipeline.set_state(gst::State::Paused),
        Err(gst::StateChangeError),
        "an invalid quad refuses to start"
    );
    let message = pipeline
        .bus()
        .expect("bus")
        .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Error])
        .expect("an error message");
    let error = match message.view() {
        gst::MessageView::Error(error) => Some(error.error()),
        _ => None,
    }
    .expect("an error");
    assert!(
        error.matches(gst::LibraryError::Settings) && error.message().contains("invalid quad"),
        "{error:?}"
    );
    let _ignored = pipeline.set_state(gst::State::Null);

    let mut gl = Warp::new("glwallwarp", &format!("quad={}", QUADS[0]), (64, 48), RGBA);
    let before = gl.frame();
    gl.element().set_property("quad", "nonsense");
    assert_eq!(gl.element().property::<String>("quad"), QUADS[0]);
    assert_eq!(gl.frame(), before);
}

#[test]
fn output_takes_any_format_or_memory_downstream_asks_for() {
    if !gl() {
        return;
    }
    let caps = |sample: &gst::Sample| {
        let caps = sample.caps().expect("caps");
        let info = gst_video::VideoInfo::from_caps(caps).expect("info");
        let gl = caps
            .features(0)
            .is_some_and(|features| features.contains("memory:GLMemory"));
        (info.format(), info.width(), info.height(), gl)
    };
    let mut i420 = Warp::new(
        "glwallwarp",
        "output-width=40 output-height=30",
        (64, 48),
        "video/x-raw,format=I420",
    );
    assert_eq!(
        caps(&i420.sample()),
        (gst_video::VideoFormat::I420, 40, 30, false)
    );
    let mut textures = Warp::new(
        "glwallwarp",
        "",
        (64, 48),
        "video/x-raw(memory:GLMemory),width=20,height=10",
    );
    assert_eq!(
        caps(&textures.sample()),
        (gst_video::VideoFormat::Rgba, 20, 10, true)
    );
    // Square pixels unless downstream wants others, as with wallwarp.
    let par = |output: &str| {
        let mut warp = Warp::new(
            "glwallwarp",
            "output-width=40 output-height=30",
            (64, 48),
            output,
        );
        let sample = warp.sample();
        gst_video::VideoInfo::from_caps(sample.caps().expect("caps"))
            .expect("info")
            .par()
    };
    assert_eq!(par(RGBA), gst::Fraction::new(1, 1));
    assert_eq!(
        par("video/x-raw,format=RGBA,pixel-aspect-ratio=4/3"),
        gst::Fraction::new(4, 3)
    );
}

#[test]
fn uploaded_input_feeds_several_displays() {
    if !gl() {
        return;
    }
    // One upload before a tee, as for a wall; each display warps its texture.
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=source format=time \
           caps=video/x-raw,format=RGBA,width=64,height=48,framerate=30/1 \
         ! glupload ! tee name=t \
         t. ! queue ! glwallwarp quad={} interpolation=nearest output-width=40 output-height=30 \
            ! {RGBA} ! appsink name=first sync=false \
         t. ! queue ! glwallwarp quad={} interpolation=nearest output-width=30 output-height=40 \
            ! {RGBA} ! appsink name=second sync=false",
        QUADS[0], QUADS[2]
    ))
    .expect("pipeline")
    .downcast::<gst::Pipeline>()
    .expect("a pipeline");
    pipeline.set_state(gst::State::Playing).expect("playing");
    let by_name = |name: &str| pipeline.by_name(name).expect(name);
    let source = by_name("source");
    for frame in 0..2 {
        push(&source, (64, 48), frame);
        for (sink, quad, size) in [
            ("first", QUADS[0], (40, 30)),
            ("second", QUADS[2], (30, 40)),
        ] {
            let mut cpu = Warp::new(
                "wallwarp",
                &format!(
                    "quad={quad} interpolation=nearest output-width={} output-height={}",
                    size.0, size.1
                ),
                (64, 48),
                RGBA,
            );
            assert_frames(
                &cpu.frame(),
                &rows(&pull(&by_name(sink))),
                Match::Nearest,
                0xff00_0000,
                &format!("{sink} display, frame {frame}"),
            );
        }
    }
    let _ignored = pipeline.set_state(gst::State::Null);
}

#[test]
fn size_changes_stay_inside_the_element() {
    if !gl() {
        return;
    }
    let mut gl = Warp::new(
        "glwallwarp",
        "output-width=40 output-height=30",
        (64, 48),
        RGBA,
    );
    gl.frame();
    // Reconfigure events reaching the source would make a whole wall's
    // pipeline query caps again for one display's new size.
    let reconfigures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&reconfigures);
    gl.source.static_pad("src").expect("source pad").add_probe(
        gst::PadProbeType::EVENT_UPSTREAM,
        move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && event.type_() == gst::EventType::Reconfigure
            {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            gst::PadProbeReturn::Ok
        },
    );
    gl.element().set_property("output-width", 24_u32);
    assert_eq!(gl.frame().0, (24, 30));
    assert_eq!(reconfigures.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn shares_wallwarps_properties() {
    init();
    let properties = |factory: &str| {
        let element = gst::ElementFactory::make(factory).build().expect(factory);
        element
            .list_properties()
            .iter()
            .filter(|spec| spec.owner_type() == element.type_())
            .map(|spec| {
                (
                    spec.name().to_owned(),
                    spec.default_value()
                        .serialize()
                        .map(|value| value.to_string())
                        .ok(),
                    spec.flags(),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut cpu = properties("wallwarp");
    // Threads split the CPU's work; the GPU has its own.
    cpu.retain(|(name, ..)| name != "n-threads");
    let mut gl = properties("glwallwarp");
    for list in [&mut cpu, &mut gl] {
        list.sort_by(|a, b| a.0.cmp(&b.0));
    }
    assert_eq!(
        gl.iter().map(|(name, ..)| name).collect::<Vec<_>>(),
        cpu.iter().map(|(name, ..)| name).collect::<Vec<_>>()
    );
    for ((name, gl_default, gl_flags), (_, cpu_default, cpu_flags)) in gl.iter().zip(&cpu) {
        assert_eq!(gl_default, cpu_default, "{name} default");
        assert_eq!(gl_flags, cpu_flags, "{name} flags");
    }
}
