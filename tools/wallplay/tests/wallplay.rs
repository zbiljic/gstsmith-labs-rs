//! The demo server end to end over HTTP, with `appsink`s standing in for
//! `webrtcsink` and the test standing in for the browsers.
#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require a working server and pipeline"
)]

use std::collections::BTreeMap;
use std::io::{Cursor, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::FromStr as _;
use std::sync::Once;
use std::time::Duration;

use gst::prelude::*;
use gst_video::prelude::*;
use serde_json::{Value, json};
use wall_geometry::{Layout, Point, Quad, Screen, Size};
use wallcalibrate::manifest::Manifest;
use wallcalibrate::synth::{self, SynthOptions};
use wallplay::app::{App, Config};
use wallplay::framecode::{self, CodeGeometry};
use wallplay::player::{self, Clock, Fill, Player, PlayerOptions, Sink, frame_number};
use wallplay::source::{DEFAULT_SOURCE, DEFAULT_STANDBY, Prepared, Resolver, Source};

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("GStreamer");
        wallplay::register_wallwarp().expect("register wallwarp");
    });
}

fn options() -> PlayerOptions {
    PlayerOptions {
        source: Prepared::pattern(DEFAULT_SOURCE).expect("pattern"),
        standby: DEFAULT_STANDBY.to_owned(),
        restart: true,
        fps: 30,
        sink: Sink::AppSink,
        fill: Fill::default(),
        resolver: Resolver::default(),
        audio: false,
    }
}

/// Start a server on a free local port.
fn serve(state_dir: &std::path::Path) -> (std::sync::Arc<App>, SocketAddr) {
    serve_polling(state_dir, Duration::from_millis(200))
}

/// Start a server whose polls wait at most `poll_timeout` for a change.
fn serve_polling(
    state_dir: &std::path::Path,
    poll_timeout: Duration,
) -> (std::sync::Arc<App>, SocketAddr) {
    init();
    let app = App::new(Config {
        state_dir: state_dir.to_owned(),
        player: options(),
        poll_timeout,
        max_output: 1920,
        sound: None,
    })
    .expect("app");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let handler = app.handler();
    std::thread::spawn(move || wallplay::http::serve(&listener, &handler));
    (app, address)
}

/// One request on its own connection; returns the status and the body.
fn request(address: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nUser-Agent: wallplay-test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response head");
    let status = std::str::from_utf8(response.get(9..12).expect("status line"))
        .expect("status")
        .parse()
        .expect("status code");
    (status, response.get(split + 4..).expect("body").to_vec())
}

fn json_request(address: SocketAddr, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let (status, body) = request(address, method, path, body.to_string().as_bytes());
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("JSON response")
    };
    (status, value)
}

fn ok(address: SocketAddr, method: &str, path: &str, body: &Value) -> Value {
    let (status, value) = json_request(address, method, path, body);
    assert!(
        (200..300).contains(&status),
        "{method} {path}: {status} {value}"
    );
    value
}

fn poll(address: SocketAddr, id: &str, width: u32, height: u32) -> Value {
    let path = format!("/api/displays/{id}/poll?since=0&w={width}&h={height}&scale=2&fullscreen=1");
    ok(address, "GET", &path, &Value::Null)
        .get("command")
        .cloned()
        .expect("command")
}

type Codes = BTreeMap<gst::ClockTime, (u64, Size)>;

/// The running time of the next frame from `sink`, with its code if it
/// carries one.
fn pull(sink: &gst::Element) -> (gst::ClockTime, Option<u64>, Size) {
    let sample = sink
        .emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::from_seconds(5)])
        .expect("sample");
    let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
    let buffer = sample.buffer().expect("buffer");
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map");
    let geometry = CodeGeometry::for_output(info.width(), info.height()).expect("geometry");
    let stride = frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
        .expect("stride");
    let code = framecode::read(frame.plane_data(0).expect("plane"), stride, &geometry);
    let size = Size {
        width: info.width(),
        height: info.height(),
    };
    let running = sample
        .segment()
        .and_then(|segment| segment.downcast_ref::<gst::format::Time>())
        .and_then(|segment| segment.to_running_time(buffer.pts()?))
        .expect("running time");
    (running, code, size)
}

/// Pull from every display's sink in turn, so their frames overlap, until
/// each has `count` frames carrying a code, keyed by running time.
fn coded_frames(pipeline: &gst::Pipeline, ids: &[&str], count: usize) -> Vec<Codes> {
    let sinks: Vec<_> = ids
        .iter()
        .map(|id| pipeline.by_name(&format!("sink-{id}")).expect("sink"))
        .collect();
    let mut frames = vec![Codes::new(); ids.len()];
    for _ in 0..count * 20 {
        for (sink, frames) in sinks.iter().zip(&mut frames) {
            if frames.len() < count
                && let (pts, Some(code), size) = pull(sink)
            {
                frames.insert(pts, (code, size));
            }
        }
        if frames.iter().all(|frames| frames.len() == count) {
            return frames;
        }
    }
    assert!(
        frames.iter().all(|frames| frames.len() == count),
        "too few coded frames: {:?}",
        frames.iter().map(Codes::len).collect::<Vec<_>>()
    );
    frames
}

/// A landscape and a portrait display on a 640x360 canvas, as JSON.
fn two_displays_json() -> String {
    json!({
        "version": 1,
        "canvas": {"width": 640, "height": 360},
        "screens": [
            {"id": "left", "output": {"width": 320, "height": 180},
             "corners": [[0, 0], [320, 0], [320, 180], [0, 180]]},
            {"id": "right", "output": {"width": 180, "height": 320},
             "corners": [[640, 0], [640, 360], [400, 360], [400, 0]]}
        ]
    })
    .to_string()
}

/// A landscape and a portrait display on a 640x360 canvas.
fn two_displays() -> Layout {
    Layout::from_json(&two_displays_json()).expect("layout")
}

/// Whether any of the next ten buffers of sound from `sink` is not silent,
/// checking that sound comes as 16-bit 48 kHz stereo in 20 ms buffers, in
/// exactly the same caps whatever plays, since `webrtcsink` refuses a change.
fn audible(sink: &gst::Element) -> bool {
    let expected = gst::Caps::from_str(
        "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2,channel-mask=(bitmask)0x3",
    )
    .expect("caps");
    let mut loud = false;
    let mut whole = 0;
    for _ in 0..10 {
        let sample = sink
            .emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(5)],
            )
            .expect("sound");
        let caps = sample.caps().expect("caps");
        assert!(caps.is_strictly_equal(&expected), "{caps}");
        let buffer = sample.buffer().expect("buffer");
        whole += usize::from(buffer.size() == 960 * 4);
        loud |= buffer
            .map_readable()
            .expect("map")
            .iter()
            .any(|byte| *byte != 0);
    }
    assert!(whole >= 8, "{whole} of 10 buffers last 20 ms");
    loud
}

/// How far the pipeline moved the stream reaching `sink`, from the running
/// time and timestamp of its next buffer, in nanoseconds.
fn moved(sink: &gst::Element) -> i128 {
    let sample = sink
        .emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::from_seconds(5)])
        .expect("sample");
    let pts = sample
        .buffer()
        .and_then(gst::BufferRef::pts)
        .expect("timestamp");
    let running = sample
        .segment()
        .and_then(|segment| segment.downcast_ref::<gst::format::Time>())
        .and_then(|segment| segment.to_running_time(pts))
        .expect("running time");
    i128::from(running.nseconds()) - i128::from(pts.nseconds())
}

#[test]
fn sound_follows_what_shows() {
    init();
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    // A clip with a tone, not live, so its sound must start with its picture
    // rather than at the start of the pipeline.
    let clip = Prepared::prepare(
        &Source::Launch("videotestsrc pattern=ball audiotestsrc freq=440".to_owned()),
        &Resolver::default(),
    )
    .expect("clip");
    let player = Player::start(
        &layout,
        &PlayerOptions {
            source: clip.clone(),
            audio: true,
            // Cropping needs gst-plugins-good, which CI does not install.
            fill: Fill::Contain,
            ..options()
        },
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    let pipeline = player.pipeline();
    let sounds = ["left", "right"].map(|id| {
        pipeline
            .by_name(&format!("sound-{id}"))
            .expect("sound sink")
    });
    assert!(eventually(10, || player.source_status().state == "playing"));
    for sound in &sounds {
        assert!(eventually(5, || audible(sound)), "the clip's tone");
    }
    // Moved to the clock as far as its picture, since both start at zero;
    // a frame of the standby, shown while the clip stalls, moved less.
    let video = pipeline.by_name("sink-left").expect("sink");
    let mut moves = (0, 0);
    assert!(
        eventually(5, || {
            moves = (moved(&sounds[0]), moved(&video));
            (moves.0 - moves.1).abs() < 50_000_000
        }),
        "sound moved {} ns, picture {} ns",
        moves.0,
        moves.1
    );
    // A pattern has no sound: the displays get silence.
    player.set_source(&Prepared::pattern("ball").expect("pattern"));
    assert!(eventually(10, || !audible(&sounds[0])), "silence");
    // So does the standby, while a source fails.
    let missing = Prepared::prepare(
        &Source::Media("file:///nonexistent/wallplay.mp4".to_owned()),
        &Resolver::default(),
    )
    .expect("missing file");
    player.set_source(&missing);
    assert!(eventually(10, || player.source_status().state == "retrying"));
    assert!(!audible(&sounds[0]), "silence on standby");
    // And the clip's sound comes back with it.
    player.set_source(&clip);
    assert!(eventually(10, || audible(&sounds[0])), "the tone again");
}

#[test]
fn sound_moves_between_displays_and_turns_on_and_off() {
    let state = tempfile::tempdir().expect("state directory");
    std::fs::write(state.path().join("layout.json"), two_displays_json()).expect("layout");
    let (app, address) = serve(state.path());
    let sound =
        |display: Value| json_request(address, "POST", "/api/sound", &json!({"display": display}));
    assert_eq!(sound(json!("a b")).0, 400);
    ok(address, "POST", "/api/play", &Value::Null);
    let command = |id| poll(address, id, 320, 180);
    let silent = command("left")["generation"].clone();
    assert_eq!(command("left")["sound"], false);
    assert!(
        app.pipeline()
            .and_then(|pipeline| pipeline.by_name("sound"))
            .is_none()
    );
    // Turning it on restarts playback: every stream gains sound.
    assert_eq!(sound(json!("left")).0, 200);
    let loud = command("left")["generation"].clone();
    assert_ne!(loud, silent);
    assert_eq!(command("left")["sound"], true);
    assert_eq!(command("right")["sound"], false);
    let pipeline = app.pipeline().expect("pipeline");
    assert!(pipeline.by_name("sound-left").is_some() && pipeline.by_name("sound-right").is_some());
    // Moving it to another display keeps the streams.
    assert_eq!(sound(json!("right")).0, 200);
    assert_eq!(command("left")["generation"], loud);
    assert_eq!(command("left")["sound"], false);
    assert_eq!(command("right")["sound"], true);
    // Turning it off restarts playback without it.
    let (status, state_json) = sound(Value::Null);
    assert_eq!(status, 200);
    assert_eq!(state_json["sound"], Value::Null);
    assert_ne!(command("left")["generation"], loud);
    assert!(
        app.pipeline()
            .and_then(|pipeline| pipeline.by_name("sound"))
            .is_none()
    );
}

/// Whether `done` becomes true within `seconds`.
fn eventually(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

#[test]
fn every_output_carries_the_same_frame_codes_while_measuring() {
    init();
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    let player =
        Player::start(&layout, &options(), &measurement, Clock::new(), |_| {}).expect("player");
    assert_eq!(player.routes().len(), 2);
    assert_eq!(
        player.route("right").expect("route").endpoint,
        player::Endpoint::None
    );
    player.set_measuring(true);
    let frames = coded_frames(player.pipeline(), &["left", "right"], 10);
    for (pts, (code, size)) in frames.iter().flatten() {
        assert_eq!(
            *code,
            frame_number(*pts, 30) % framecode::MODULUS,
            "at {pts}"
        );
        assert!(matches!((size.width, size.height), (320, 180) | (180, 320)));
    }
    let summary = measurement
        .lock()
        .expect("measurement")
        .summary(&["left".to_owned(), "right".to_owned()]);
    // Nothing presented anything yet; stamps alone make no samples.
    assert!(summary.displays.iter().all(|display| display.frames == 0));
}

#[test]
fn pages_of_an_earlier_server_run_hear_back_at_once() {
    let state = tempfile::tempdir().expect("state directory");
    let (_app, address) = serve_polling(state.path(), Duration::from_secs(20));
    let first = ok(
        address,
        "GET",
        "/api/displays/left/poll?since=0",
        &Value::Null,
    );
    let epoch = first["epoch"].as_str().expect("epoch").to_owned();
    // A version from an earlier run, far ahead of this one's, with that
    // run's epoch: the page would otherwise wait for the whole poll.
    let started = std::time::Instant::now();
    let reply = ok(
        address,
        "GET",
        "/api/displays/left/poll?since=999&epoch=earlier",
        &Value::Null,
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(reply["epoch"], epoch.as_str());
    assert_eq!(reply["command"]["mode"], "idle");
    // So does a page without an epoch whose version is ahead of this run's.
    let started = std::time::Instant::now();
    ok(
        address,
        "GET",
        "/api/displays/left/poll?since=999",
        &Value::Null,
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // A page of this run still waits for a change; showing patterns is one.
    let version = reply["version"].as_u64().expect("version");
    let waiting = std::thread::spawn(move || {
        let path = format!("/api/displays/left/poll?since={version}&epoch={epoch}&w=640&h=360");
        let started = std::time::Instant::now();
        let reply = ok(address, "GET", &path, &Value::Null);
        (started.elapsed(), reply)
    });
    std::thread::sleep(Duration::from_millis(500));
    ok(address, "POST", "/api/patterns", &Value::Null);
    let (waited, reply) = waiting.join().expect("poll");
    assert!(waited >= Duration::from_millis(400), "{waited:?}");
    assert_eq!(reply["command"]["mode"], "pattern");
}

#[test]
fn requests_out_of_order_are_refused_with_a_reason() {
    let state = tempfile::tempdir().expect("state directory");
    let (_app, address) = serve(state.path());
    assert_eq!(
        json_request(address, "POST", "/api/play", &Value::Null).0,
        409
    );
    assert_eq!(
        json_request(address, "POST", "/api/patterns", &Value::Null).0,
        409
    );
    assert_eq!(
        request(address, "POST", "/api/photo", b"\xff\xd8\xff").0,
        409
    );
    assert_eq!(
        json_request(address, "GET", "/api/displays/a%20b/poll", &Value::Null).0,
        400
    );
    assert_eq!(
        json_request(address, "GET", "/nowhere", &Value::Null).0,
        404
    );
    let (status, page) = request(address, "GET", "/display/lobby", b"");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&page).contains(r#"const id = "lobby";"#));
}

/// A synthetic photo of the server's displays showing their patterns, as PNG,
/// with where each display appears in it.
fn synthetic_photo(address: SocketAddr) -> (Vec<u8>, [(String, Quad); 2]) {
    let manifest = Manifest::from_json(
        &String::from_utf8(request(address, "GET", "/api/manifest", b"").1).expect("UTF-8"),
    )
    .expect("manifest");
    let quad = |points: [(f64, f64); 4]| Quad::new(points.map(|(x, y)| Point::new(x, y)));
    let placements = [
        (
            "left".to_owned(),
            quad([
                (100.0, 150.0),
                (560.0, 170.0),
                (555.0, 430.0),
                (105.0, 420.0),
            ]),
        ),
        (
            "right".to_owned(),
            quad([
                (700.0, 100.0),
                (1000.0, 110.0),
                (990.0, 640.0),
                (705.0, 650.0),
            ]),
        ),
    ];
    let options = SynthOptions {
        width: 1200,
        height: 800,
        ..SynthOptions::default()
    };
    let photo = synth::render(&manifest, &placements, &options).expect("photo");
    let mut png = Cursor::new(Vec::new());
    photo
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("PNG");
    (png.into_inner(), placements)
}

/// Coded frames pulled from display `id` of the app's running pipeline.
fn codes(app: &App, id: &str) -> usize {
    let pipeline = app.pipeline().expect("pipeline");
    coded_frames(&pipeline, &[id], 3)
        .first()
        .map_or(0, BTreeMap::len)
}

#[test]
fn sources_end_fail_and_change_while_playing() {
    init();
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    let resolver = Resolver::default();
    // A source that is not live and ends after a second and a half.
    let clip = Prepared::prepare(
        &Source::Launch("videotestsrc num-buffers=45 pattern=ball".to_owned()),
        &resolver,
    )
    .expect("clip");
    let player = Player::start(
        &layout,
        &PlayerOptions {
            source: clip.clone(),
            restart: false,
            // Cropping needs gst-plugins-good, which CI does not install.
            fill: Fill::Contain,
            ..options()
        },
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    let sink = player.pipeline().by_name("sink-left").expect("sink");
    assert!(eventually(10, || player.source_status().state == "playing"));
    assert!(!player.source_status().standby);
    // Played once, it ends, and the standby takes over.
    assert!(eventually(10, || player.source_status().state == "ended"));
    assert!(player.source_status().standby);
    for _ in 0..5 {
        pull(&sink);
    }
    // Restarted, it plays again after ending, its last frame staying until
    // the next run's first rather than the standby showing.
    player.set_restart(true);
    player.set_source(&clip);
    assert!(eventually(10, || player.source_status().state == "playing"));
    let mut standby = false;
    assert!(eventually(15, || {
        standby |= player.source_status().standby;
        player.source_status().restarts >= 2
    }));
    assert!(!standby, "the standby showed while the clip looped");
    // Another source replaces it the same way.
    player.set_source(&Prepared::pattern("ball").expect("pattern"));
    assert!(eventually(10, || {
        standby |= player.source_status().standby;
        player.source_status().state == "playing"
    }));
    assert!(!standby, "the standby showed while switching sources");
    // The sources it replaced are gone.
    let sources = || {
        player
            .pipeline()
            .children()
            .iter()
            .filter(|child| child.name().starts_with("input-source-"))
            .count()
    };
    assert!(eventually(5, || sources() == 1), "{} sources", sources());
    // A source that fails is retried, with the reason, on standby.
    let missing = Prepared::prepare(
        &Source::Media("file:///nonexistent/wallplay.mp4".to_owned()),
        &resolver,
    )
    .expect("missing file");
    player.set_source(&missing);
    assert!(eventually(10, || player.source_status().state == "retrying"));
    let status = player.source_status();
    assert!(status.standby);
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("/nonexistent/wallplay.mp4")),
        "{status:?}"
    );
    for _ in 0..5 {
        pull(&sink);
    }
    // Another source replaces it without stopping the pipeline.
    player.set_source(&Prepared::pattern("checkerboard").expect("pattern"));
    assert!(eventually(10, || {
        let status = player.source_status();
        status.state == "playing" && !status.standby && status.error.is_none()
    }));
    player.set_measuring(true);
    for (pts, (code, _)) in coded_frames(player.pipeline(), &["left"], 5)
        .iter()
        .flatten()
    {
        assert_eq!(*code, frame_number(*pts, 30) % framecode::MODULUS);
    }
}

#[test]
fn layouts_change_while_playing() {
    init();
    playing_on_a_new_layout(Fill::Contain);
    // Cropping needs gst-plugins-good, which CI does not install.
    if gst::ElementFactory::find("videocrop").is_some() {
        playing_on_a_new_layout(Fill::Cover);
    }
}

#[test]
fn covering_crops_follow_the_canvas() {
    init();
    if gst::ElementFactory::find("videocrop").is_none() {
        return;
    }
    // A 4:3 source that keeps its size whatever the canvas.
    let clip = Prepared::prepare(
        &Source::Launch(
            "videotestsrc pattern=ball ! capsfilter caps=video/x-raw,width=640,height=480"
                .to_owned(),
        ),
        &Resolver::default(),
    )
    .expect("clip");
    let measurement = std::sync::Arc::default();
    let player = Player::start(
        &two_displays(),
        &PlayerOptions {
            source: clip,
            fill: Fill::Cover,
            ..options()
        },
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    let ok = eventually(10, || player.source_status().state == "playing");
    assert!(ok, "{:?}", player.source_status());
    let crop = player.pipeline().by_name("crop").expect("crop");
    let rows = |crop: &gst::Element| {
        let sides = ["top", "bottom", "left", "right"].map(|side| crop.property::<i32>(side));
        (sides[0], sides[1], sides[2], sides[3])
    };
    // 480 rows of a 640-wide 16:9 picture keep 360.
    assert!(eventually(10, || rows(&crop) == (60, 60, 0, 0)));
    let mut narrower: serde_json::Value =
        serde_json::from_str(&two_displays().to_json().expect("json")).expect("json");
    narrower["canvas"]["height"] = json!(300);
    let narrower = Layout::from_json(&narrower.to_string()).expect("layout");
    assert!(player.update_layout(&narrower, Fill::Cover));
    // And 300 of a 640x300 one.
    assert!(eventually(10, || rows(&crop) == (90, 90, 0, 0)));
}

fn playing_on_a_new_layout(fill: Fill) {
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    // A source that is not live keeps playing through a new canvas size.
    let clip = Prepared::prepare(
        &Source::Launch("videotestsrc pattern=ball".to_owned()),
        &Resolver::default(),
    )
    .expect("clip");
    let player = Player::start(
        &layout,
        &PlayerOptions {
            source: clip,
            fill,
            ..options()
        },
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    assert!(eventually(10, || player.source_status().state == "playing"));
    let source = player.pipeline().by_name("input-source-1").expect("source");
    // The same displays on a narrower canvas, the left one cropped.
    let narrower = Layout::from_json(
        &json!({
            "version": 1,
            "canvas": {"width": 640, "height": 300},
            "screens": [
                {"id": "left", "output": {"width": 320, "height": 180},
                 "corners": [[40, 20], [320, 20], [320, 180], [40, 180]]},
                {"id": "right", "output": {"width": 180, "height": 320},
                 "corners": [[640, 0], [640, 300], [400, 300], [400, 0]]}
            ]
        })
        .to_string(),
    )
    .expect("layout");
    assert!(player.update_layout(&narrower, fill));
    let warp = player.pipeline().by_name("warp-left").expect("warp");
    let quad = narrower
        .source_quad(narrower.screen("left").expect("left"))
        .to_string();
    // The quad follows the frames of the new size: it changes as their caps
    // arrive, before the element takes them.
    let size = || {
        let caps = warp.static_pad("sink")?.current_caps()?;
        let info = gst_video::VideoInfo::from_caps(&caps).ok()?;
        Some((info.width(), info.height()))
    };
    assert!(eventually(10, || size() == Some((640, 300))));
    assert_eq!(warp.property::<String>("quad"), quad);
    // The source kept playing rather than starting over.
    assert_eq!(
        player.pipeline().by_name("input-source-1").as_ref(),
        Some(&source)
    );
    assert!(player.pipeline().by_name("input-source-2").is_none());
    assert_eq!(player.source_status().restarts, 0);
    player.set_measuring(true);
    for (pts, (code, size)) in coded_frames(player.pipeline(), &["left", "right"], 5)
        .iter()
        .flatten()
    {
        assert_eq!(*code, frame_number(*pts, 30) % framecode::MODULUS);
        assert!(matches!((size.width, size.height), (320, 180) | (180, 320)));
    }
    // Another stream size needs a new pipeline.
    let resized = Layout::from_json(
        &two_displays()
            .to_json()
            .expect("JSON")
            .replace(r#""width": 320"#, r#""width": 160"#)
            .replace(r#""height": 180"#, r#""height": 90"#),
    )
    .expect("layout");
    assert_ne!(resized, two_displays());
    assert!(!player.update_layout(&resized, fill));
}

#[test]
fn sources_and_buffers_are_checked() {
    let state = tempfile::tempdir().expect("state directory");
    let (_app, address) = serve(state.path());
    let status = |path: &str, body: Value| json_request(address, "POST", path, &body).0;
    // Pipelines could write files, so only the command line may set them.
    assert_eq!(
        status(
            "/api/source",
            json!({"launch": "videotestsrc ! filesink location=x"})
        ),
        400
    );
    assert_eq!(status("/api/source", json!({"pattern": "plaid"})), 400);
    assert_eq!(status("/api/source", json!({"uri": "video.mp4"})), 400);
    assert_eq!(status("/api/source", json!({"page": "watch?v=x"})), 400);
    assert_eq!(status("/api/source", json!({"stream": "rtsp://x"})), 400);
    assert_eq!(
        status("/api/source", json!({"uri": "file:///video.mp4"})),
        200
    );
    assert_eq!(status("/api/playback", json!({"standby": "plaid"})), 400);
    assert_eq!(
        status(
            "/api/playback",
            json!({"standby": "black", "restart": false})
        ),
        200
    );
    assert_eq!(status("/api/buffer", json!({"ms": 5000})), 400);
    assert_eq!(status("/api/buffer", json!({"ms": 50})), 200);
    let state_json = ok(address, "GET", "/api/state", &Value::Null);
    assert_eq!(
        state_json["source"]["source"],
        json!({"uri": "file:///video.mp4"})
    );
    assert_eq!(state_json["source"]["via"], "gstreamer");
    assert_eq!(state_json["standby"], "black");
    assert_eq!(state_json["restart"], false);
    assert_eq!(state_json["buffer_ms"], 50);
    let patterns = state_json["patterns"].as_array().expect("patterns");
    assert_eq!(patterns[0]["id"], "bars");
    assert!(
        patterns
            .iter()
            .all(|pattern| pattern.get("launch").is_none())
    );
}

/// Register the two test displays, show their patterns, and calibrate them
/// from a synthetic photo.
fn calibrate(address: SocketAddr) {
    poll(address, "left", 480, 270);
    poll(address, "right", 270, 480);
    ok(address, "POST", "/api/patterns", &Value::Null);
    let (png, _) = synthetic_photo(address);
    let (status, body) = request(address, "POST", "/api/photo", &png);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
}

#[test]
fn aligning_delays_the_early_display() {
    let state = tempfile::tempdir().expect("state directory");
    let (app, address) = serve(state.path());
    let align = |on: bool| json_request(address, "POST", "/api/align", &json!({"on": on}));
    assert_eq!(align(true).0, 409, "nothing plays yet");
    calibrate(address);
    ok(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "viewpoint", "canvas": [1280, 720]}),
    );
    ok(address, "POST", "/api/play", &Value::Null);
    let (status, started) = align(true);
    assert_eq!(status, 200);
    assert_eq!(started["aligning"]["steps"], 0);
    // Aligning records frames and has the browsers report them.
    assert_eq!(poll(address, "left", 480, 270)["measuring"], true);
    // A newest frame with 60 before it.
    let sink = app
        .pipeline()
        .and_then(|pipeline| pipeline.by_name("sink-left"))
        .expect("sink");
    let latest = std::iter::repeat_with(|| frame_number(pull(&sink).0, 30))
        .find(|frame| *frame >= 60)
        .expect("frames");
    // Stand in for browsers that tell frames apart by RTP timestamp: the
    // right display presents every frame 50 ms after the left, from after
    // the aligner's settling time.
    let now = ok(address, "POST", "/api/clock", &Value::Null)["now"]
        .as_f64()
        .expect("now");
    for (id, late) in [("left", 0.0), ("right", 50.0)] {
        let frames: Vec<Value> = (0..60_u32)
            .map(|index| {
                let frame = latest - 59 + u64::from(index);
                let shown = now + 2500.0 + f64::from(index) * 1000.0 / 30.0 + late;
                json!([(frame * 3000) % (1 << 32), shown, null])
            })
            .collect();
        let report = json!({"rtp": frames, "rtp_timestamps": true});
        let path = format!("/api/displays/{id}/frames");
        assert_eq!(json_request(address, "POST", &path, &report).0, 204);
    }
    // The left display gets most of the 50 ms in one step.
    assert!(eventually(10, || {
        poll(address, "left", 480, 270)["buffer_ms"]
            .as_u64()
            .is_some_and(|ms| (30..=40).contains(&ms))
    }));
    assert_eq!(poll(address, "right", 270, 480)["buffer_ms"], Value::Null);
    let measured = ok(address, "GET", "/api/measure", &Value::Null);
    assert_eq!(measured["aligning"]["steps"], 1);
    assert_eq!(measured["aligning"]["aligned"], false);
    // Stopping takes the delay back.
    assert_eq!(align(false).0, 200);
    assert_eq!(poll(address, "left", 480, 270)["buffer_ms"], Value::Null);
    assert_eq!(poll(address, "left", 480, 270)["measuring"], false);
}

#[test]
fn layouts_fill_the_canvas_level_and_straighten() {
    let state = tempfile::tempdir().expect("state directory");
    let (_app, address) = serve(state.path());
    calibrate(address);
    let layout = |request: Value| {
        let state = ok(address, "POST", "/api/layout", &request);
        let layout = state.get("layout").cloned().expect("layout");
        assert_eq!(
            layout
                .get("options")
                .and_then(|options| options.get("mode")),
            request.get("mode")
        );
        layout
    };
    // A tight canvas is exactly the displays' extent.
    let tight = layout(json!({"mode": "viewpoint", "canvas": {"longest": 1000}}));
    let canvas = tight.get("canvas").cloned().expect("canvas");
    let size = |name: &str| canvas.get(name).and_then(Value::as_u64).expect("size");
    assert_eq!(size("width").max(size("height")), 1000);
    let stored = Layout::from_json(
        &String::from_utf8(request(address, "GET", "/api/layout", b"").1).expect("UTF-8"),
    )
    .expect("layout");
    let (mut low, mut high) = (
        Point::new(f64::MAX, f64::MAX),
        Point::new(f64::MIN, f64::MIN),
    );
    for corner in stored
        .screens
        .iter()
        .flat_map(|screen| screen.corners.corners())
    {
        low = Point::new(low.x.min(corner.x), low.y.min(corner.y));
        high = Point::new(high.x.max(corner.x), high.y.max(corner.y));
    }
    assert!(low.x.abs() < 1e-6 && low.y.abs() < 1e-6, "{low:?}");
    assert!(
        (high.x - f64::from(stored.canvas.width)).abs() < 1.0,
        "{high:?}"
    );
    assert!(
        (high.y - f64::from(stored.canvas.height)).abs() < 1.0,
        "{high:?}"
    );

    // Leveled to the left display and straightened: upright crops.
    layout(json!({
        "mode": "viewpoint", "anchor": "left", "straighten": true,
        "canvas": {"longest": 1000}, "fill": "contain",
    }));
    let straight = Layout::from_json(
        &String::from_utf8(request(address, "GET", "/api/layout", b"").1).expect("UTF-8"),
    )
    .expect("layout");
    for screen in &straight.screens {
        let [top_left, top_right, bottom_right, bottom_left] = screen.corners.corners();
        assert!(
            (top_left.y - top_right.y).abs() < 1e-6 || (top_left.x - top_right.x).abs() < 1e-6,
            "{} is upright: {:?}",
            screen.id,
            screen.corners
        );
        assert!(
            (bottom_right.x - top_right.x).abs() < 1e-6
                || (bottom_right.y - top_right.y).abs() < 1e-6
        );
        assert!(
            (bottom_left.x - top_left.x).abs() < 1e-6 || (bottom_left.y - top_left.y).abs() < 1e-6
        );
    }
    // The fixed-size canvas still works, and planar mode needs an anchor.
    let fixed = layout(json!({"mode": "viewpoint", "canvas": [1280, 720]}));
    assert_eq!(
        fixed.get("canvas"),
        Some(&json!({"width": 1280, "height": 720}))
    );
    let (status, error) = json_request(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "planar", "canvas": {"longest": 1000}}),
    );
    assert_eq!(status, 400, "{error}");
    layout(json!({"mode": "planar", "anchor": "left", "canvas": {"longest": 1000}}));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario from registration to measurement, step by step"
)]
fn the_whole_workflow_runs_over_http() {
    let state = tempfile::tempdir().expect("state directory");
    let (app, address) = serve(state.path());

    // Two displays register, one landscape and one portrait.
    assert_eq!(poll(address, "left", 480, 270)["mode"], "idle");
    assert_eq!(poll(address, "right", 270, 480)["mode"], "idle");
    let state_json = ok(address, "GET", "/api/state", &Value::Null);
    assert_eq!(state_json["displays"][0]["viewport"]["width"], 480);
    assert_eq!(
        state_json["displays"][1]["output"],
        json!({"width": 270, "height": 480})
    );

    // Patterns sized to each viewport.
    assert_eq!(
        ok(address, "POST", "/api/patterns", &Value::Null)["displays"],
        json!(["left", "right"])
    );
    let command = poll(address, "left", 480, 270);
    assert_eq!(command["mode"], "pattern");
    assert_eq!(command["output"], json!({"width": 480, "height": 270}));
    let (status, svg) = request(
        address,
        "GET",
        command["pattern"].as_str().expect("url"),
        b"",
    );
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&svg).starts_with("<svg"));

    // A synthetic photo of both displays showing their patterns.
    let (png, placements) = synthetic_photo(address);
    let (status, body) = request(address, "POST", "/api/photo", &png);
    let calibrated: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(status, 200, "{calibrated}");
    assert_eq!(calibrated["calibration"]["complete"], true);
    let calibration = ok(address, "GET", "/api/calibration", &Value::Null);
    for (screen, (_, truth)) in calibration["screens"]
        .as_array()
        .expect("screens")
        .iter()
        .zip(&placements)
    {
        assert_eq!(screen["status"], "complete");
        for (corner, expected) in screen["corners"]
            .as_array()
            .expect("corners")
            .iter()
            .zip(truth.corners())
        {
            let (x, y) = (
                corner[0].as_f64().expect("x"),
                corner[1].as_f64().expect("y"),
            );
            assert!(
                (x - expected.x).hypot(y - expected.y) < 1.0,
                "{corner} != {expected:?}"
            );
        }
    }
    let (status, editor) = request(address, "GET", "/calibration", b"");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&editor).contains("data:image/jpeg;base64,"));

    // Corrections from the editor replace the calibration.
    let mut corrected = calibration.clone();
    corrected["screens"][1]["status"] = json!("manual");
    corrected["screens"][1]["corners"][0] = json!([701.0, 101.0]);
    ok(address, "PUT", "/api/calibration", &corrected);
    assert_eq!(
        ok(address, "GET", "/api/state", &Value::Null)["calibration"]["screens"][1]["status"],
        "manual"
    );
    let mut mirrored = corrected.clone();
    mirrored["screens"][0]["corners"] = json!([[100, 150], [105, 420], [555, 430], [560, 170]]);
    assert_eq!(
        json_request(address, "PUT", "/api/calibration", &mirrored).0,
        422
    );

    // A planar layout needs a reference; then the layout is made and saved.
    let (status, error) = json_request(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "planar", "canvas": [1280, 720]}),
    );
    assert_eq!(status, 400, "{error}");
    let made = ok(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "viewpoint", "canvas": [1280, 720]}),
    );
    assert_eq!(
        made["layout"]["canvas"],
        json!({"width": 1280, "height": 720})
    );
    let layout = Layout::from_json(
        &String::from_utf8(request(address, "GET", "/api/layout", b"").1).expect("UTF-8"),
    )
    .expect("layout");
    assert_eq!(
        layout
            .screens
            .iter()
            .map(|screen: &Screen| (screen.id.as_str(), screen.output))
            .collect::<Vec<_>>(),
        [
            (
                "left",
                Size {
                    width: 480,
                    height: 270
                }
            ),
            (
                "right",
                Size {
                    width: 270,
                    height: 480
                }
            )
        ]
    );
    assert!(state.path().join("layout.json").exists() && state.path().join("photo.png").exists());

    // Playing gives every display its route.
    assert_eq!(
        json_request(address, "POST", "/api/measure", &json!({"on": true})).0,
        409
    );
    ok(address, "POST", "/api/play", &Value::Null);
    let command = poll(address, "right", 270, 480);
    assert_eq!(command["mode"], "play");
    assert_eq!(command["endpoint"], "none");
    assert_eq!(command["code"]["bits"], framecode::BITS);
    assert_eq!(command["measuring"], false);
    // The standby shows until the source's first frame.
    assert!(eventually(10, || {
        let source = &ok(address, "GET", "/api/state", &Value::Null)["player"]["source"];
        source["state"] == "playing" && source["standby"] == false
    }));
    ok(address, "POST", "/api/measure", &json!({"on": true}));
    assert_eq!(poll(address, "right", 270, 480)["measuring"], true);

    // Stand in for the browsers: read the codes of frames both displays
    // present, and report them shown 50 ms after the clock reading, the right
    // display 12 ms later than the left.
    let pipeline = app.pipeline().expect("pipeline");
    let [left, right] = <[Codes; 2]>::try_from(coded_frames(&pipeline, &["left", "right"], 30))
        .expect("two displays");
    for (pts, (code, _)) in left.iter().chain(&right) {
        assert_eq!(
            *code,
            frame_number(*pts, 30) % framecode::MODULUS,
            "at {pts}"
        );
    }
    let now = ok(address, "POST", "/api/clock", &Value::Null)["now"]
        .as_f64()
        .expect("now");
    let common: Vec<_> = left.keys().filter(|pts| right.contains_key(pts)).collect();
    // Each branch's leaky queue may drop different frames under load.
    assert!(common.len() >= 3, "only {} frames in common", common.len());
    for (id, codes, delay) in [("left", &left, 50.0), ("right", &right, 62.0)] {
        let frames: Vec<Value> = common
            .iter()
            .map(|pts| {
                let ms = f64::from(u32::try_from(pts.mseconds()).expect("short test"));
                json!([codes[pts].0, now + delay + ms, null])
            })
            .collect();
        let report = json!({"rtt": 2.0, "unreadable": 1, "frames": frames});
        assert_eq!(
            json_request(
                address,
                "POST",
                &format!("/api/displays/{id}/frames"),
                &report
            )
            .0,
            204
        );
    }
    let measured = ok(address, "GET", "/api/measure", &Value::Null);
    let summary = &measured["summary"];
    assert_eq!(summary["reference"], "left");
    let right_summary = &summary["displays"][1];
    assert_eq!(right_summary["frames"], common.len());
    assert_eq!(right_summary["unreadable"], 1);
    assert!(
        (right_summary["offset_ms"]["median"]
            .as_f64()
            .expect("offset")
            - 12.0)
            .abs()
            < 1e-6
    );
    assert!(
        right_summary["latency_ms"]["median"]
            .as_f64()
            .expect("latency")
            .is_finite()
    );
    assert!((summary["spread_ms"]["max"].as_f64().expect("spread") - 12.0).abs() < 1e-6);

    // Playing again numbers frames from zero, so the earlier run's frames
    // are forgotten while measuring goes on.
    ok(address, "POST", "/api/play", &Value::Null);
    let restarted = &ok(address, "GET", "/api/measure", &Value::Null)["summary"]["displays"][1];
    assert_eq!(restarted["frames"], 0);
    assert_eq!(restarted["unreadable"], 0);
    assert_eq!(poll(address, "right", 270, 480)["measuring"], true);

    // The simulation's photo, and each display's output scaled down.
    let (status, photo) = request(address, "GET", "/api/photo.jpg", b"");
    assert_eq!(status, 200);
    assert!(photo.starts_with(&[0xff, 0xd8]), "a JPEG photo");
    let preview = (0..50)
        .find_map(|_| {
            let (status, body) = request(address, "GET", "/api/displays/right/preview.jpg", b"");
            if status == 200 {
                return Some(body);
            }
            std::thread::sleep(Duration::from_millis(100));
            None
        })
        .expect("a preview");
    let decoded = image::load_from_memory(&preview).expect("JPEG preview");
    // The portrait display's 270x480 output, within 480 pixels.
    assert_eq!((decoded.width(), decoded.height()), (270, 480));

    // A buffer target and a new source reach the displays without
    // restarting playback.
    let before = poll(address, "right", 270, 480);
    ok(address, "POST", "/api/buffer", &json!({"ms": 0}));
    let buffered = poll(address, "right", 270, 480);
    assert_eq!(buffered["buffer_ms"], 0);
    assert_eq!(buffered["generation"], before["generation"]);
    ok(
        address,
        "POST",
        "/api/source",
        &json!({"pattern": "checkerboard"}),
    );
    let switched = poll(address, "right", 270, 480);
    assert_eq!(switched["mode"], "play");
    assert_eq!(switched["generation"], before["generation"]);
    let state_json = ok(address, "GET", "/api/state", &Value::Null);
    assert_eq!(
        state_json["source"]["source"],
        json!({"pattern": "checkerboard"})
    );
    assert!(eventually(10, || {
        ok(address, "GET", "/api/state", &Value::Null)["player"]["source"]["state"] == "playing"
    }));
    assert!(
        codes(&app, "left") > 0,
        "the checkerboard plays and is stamped"
    );
    // So does a new layout for the same displays; playing again restarts.
    let laid_out = ok(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "viewpoint", "canvas": {"longest": 1280}}),
    );
    assert_ne!(
        laid_out["layout"]["canvas"],
        json!({"width": 1280, "height": 720})
    );
    assert_eq!(
        poll(address, "right", 270, 480)["generation"],
        before["generation"]
    );
    ok(address, "POST", "/api/play", &Value::Null);
    assert!(
        poll(address, "right", 270, 480)["generation"].as_u64() > before["generation"].as_u64()
    );

    // Stopping returns every display to idle.
    ok(address, "POST", "/api/idle", &Value::Null);
    assert_eq!(poll(address, "left", 480, 270)["mode"], "idle");
    assert!(app.pipeline().is_none());

    // A restarted server picks the calibration files back up.
    let (_, restarted) = serve(state.path());
    let state_json = ok(restarted, "GET", "/api/state", &Value::Null);
    assert_eq!(state_json["layout"]["canvas"], laid_out["layout"]["canvas"]);
    assert_eq!(state_json["calibration"]["screens"][1]["status"], "manual");
}
