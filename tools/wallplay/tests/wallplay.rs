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
use wallplay::auth::ControlToken;
use wallplay::drift::Measurement;
use wallplay::framecode::{self, CodeGeometry};
use wallplay::player::{self, Clock, Fill, Player, PlayerOptions, Sink, Warp, frame_number};
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
        warp: Warp::Cpu,
    }
}

fn options_with(warp: Warp) -> PlayerOptions {
    PlayerOptions { warp, ..options() }
}

/// Whether warping on the GPU works here, saying so when it does not.
fn gpu() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    init();
    *WORKS.get_or_init(|| {
        player::check_gpu_warp()
            .inspect_err(|reason| eprintln!("skipping: warping on the GPU does not work: {reason}"))
            .is_ok()
    })
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
    serve_with(state_dir, poll_timeout, None)
}

/// Start a server that requires `control_token`, if any, for its controls.
fn serve_with(
    state_dir: &std::path::Path,
    poll_timeout: Duration,
    control_token: Option<ControlToken>,
) -> (std::sync::Arc<App>, SocketAddr) {
    init();
    let app = App::new(Config {
        state_dir: state_dir.to_owned(),
        player: options(),
        poll_timeout,
        max_output: 1920,
        sound: None,
        control_token,
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
    let (status, _, body) = request_with(address, method, path, &[], body);
    (status, body)
}

/// One request with more headers, each a full `Name: value` line; returns
/// the status, the response head, and the body.
fn request_with(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let extra = headers
        .iter()
        .fold(String::new(), |extra, line| extra + line + "\r\n");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nUser-Agent: wallplay-test\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
    let head = String::from_utf8_lossy(response.get(..split).expect("head")).into_owned();
    (
        status,
        head,
        response.get(split + 4..).expect("body").to_vec(),
    )
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
    let packing = match info.format() {
        gst_video::VideoFormat::I420 => framecode::Packing::Luma,
        _ => framecode::Packing::Rgba,
    };
    let code = framecode::read(
        frame.plane_data(0).expect("plane"),
        stride,
        packing,
        &geometry,
    );
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
///
/// Only frames the pipeline stamped count: frames still queued from before
/// measuring started carry no code, but a moving pattern can read as one.
fn coded_frames(
    pipeline: &gst::Pipeline,
    measurement: &std::sync::Mutex<Measurement>,
    ids: &[&str],
    count: usize,
) -> Vec<Codes> {
    let sinks: Vec<_> = ids
        .iter()
        .map(|id| pipeline.by_name(&format!("sink-{id}")).expect("sink"))
        .collect();
    let mut frames = vec![Codes::new(); ids.len()];
    let stamped = |id: &str, pts| {
        measurement
            .lock()
            .expect("measurement")
            .was_stamped(id, frame_number(pts, 30))
    };
    for _ in 0..count * 20 {
        for ((sink, frames), id) in sinks.iter().zip(&mut frames).zip(ids) {
            if frames.len() < count
                && let (pts, Some(code), size) = pull(sink)
                && stamped(id, pts)
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
    let expected = gst::Caps::from_str(player::AUDIO_CAPS).expect("caps");
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
    sound_follows_what_shows_with(Warp::Cpu);
}

#[test]
fn sound_follows_what_shows_on_the_gpu() {
    if gpu() {
        sound_follows_what_shows_with(Warp::Gpu);
    }
}

fn sound_follows_what_shows_with(warp: Warp) {
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
            ..options_with(warp)
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
    every_output_carries_the_same_frame_codes_while_measuring_with(Warp::Cpu);
}

#[test]
fn every_output_carries_the_same_frame_codes_while_measuring_on_the_gpu() {
    if gpu() {
        every_output_carries_the_same_frame_codes_while_measuring_with(Warp::Gpu);
    }
}

fn every_output_carries_the_same_frame_codes_while_measuring_with(warp: Warp) {
    init();
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    let player = Player::start(
        &layout,
        &options_with(warp),
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    assert_eq!(player.routes().len(), 2);
    assert_eq!(
        player.route("right").expect("route").endpoint,
        player::Endpoint::None
    );
    player.set_measuring(true);
    let frames = coded_frames(player.pipeline(), &measurement, &["left", "right"], 10);
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
    // Without a control token, the controls are open, as they always were,
    // and there is nothing to sign in to.
    let (status, page) = request(address, "GET", "/", b"");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&page).contains("/api/state"));
    assert_eq!(request(address, "POST", "/login", b"token=x").0, 404);
}

const CONTROL_TOKEN: &str = "test-token-0123456789";

/// Every request of the controller, each of which needs the token when the
/// server has one.
fn controller_requests() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("GET", "/", Value::Null),
        ("GET", "/?token=wrong-token-0123456789", Value::Null),
        ("GET", "/calibration", Value::Null),
        ("GET", "/api/state", Value::Null),
        ("DELETE", "/api/displays/left", Value::Null),
        ("POST", "/api/patterns", Value::Null),
        ("GET", "/api/manifest", Value::Null),
        ("POST", "/api/photo", json!("not a photo")),
        ("GET", "/api/photo.jpg", Value::Null),
        ("GET", "/api/displays/left/preview.jpg", Value::Null),
        ("GET", "/api/calibration", Value::Null),
        ("PUT", "/api/calibration", json!({})),
        ("POST", "/api/layout", json!({"canvas": [1920, 1080]})),
        ("GET", "/api/layout", Value::Null),
        ("GET", "/api/layout.svg", Value::Null),
        ("POST", "/api/play", Value::Null),
        ("POST", "/api/idle", Value::Null),
        ("GET", "/api/devices", Value::Null),
        ("POST", "/api/source", json!({"uri": "file:///etc/hosts"})),
        ("POST", "/api/source", json!({"camera": "0"})),
        (
            "POST",
            "/api/source",
            json!({"launch": "videotestsrc ! fakesink"}),
        ),
        ("POST", "/api/playback", json!({"restart": false})),
        ("POST", "/api/buffer", json!({"ms": 100})),
        ("POST", "/api/sound", json!({"display": "left"})),
        ("POST", "/api/measure", json!({"on": true})),
        ("GET", "/api/measure", Value::Null),
        ("POST", "/api/align", json!({"on": true})),
        ("GET", "/nowhere", Value::Null),
        ("GET", "/login", Value::Null),
    ]
}

#[test]
fn controls_need_the_token_and_displays_do_not() {
    let state = tempfile::tempdir().expect("state directory");
    let token = ControlToken::new(CONTROL_TOKEN).expect("token");
    let (_app, address) = serve_with(state.path(), Duration::from_millis(200), Some(token));
    let bearer = format!("Authorization: Bearer {CONTROL_TOKEN}");
    let cookie = format!("Cookie: wallplay_control={CONTROL_TOKEN}");

    // Displays need nothing: their page, their poll, the clock, and frame
    // reports work as before.
    let (status, page) = request(address, "GET", "/display/left", b"");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&page).contains(r#"const id = "left";"#));
    assert_eq!(request(address, "GET", "/display", b"").0, 200);
    assert_eq!(poll(address, "left", 640, 360)["mode"], "idle");
    let now = ok(address, "POST", "/api/clock", &Value::Null)["now"]
        .as_f64()
        .expect("now");
    let report = json!({"rtt": 1.0, "frames": [[1, now, null]]});
    assert_eq!(
        json_request(address, "POST", "/api/displays/left/frames", &report).0,
        204
    );
    // A pattern that does not exist yet is missing, not forbidden.
    assert_eq!(
        request(address, "GET", "/api/displays/left/pattern.svg", b"").0,
        404
    );

    // Every controller request is refused without the token or with a
    // wrong one, before it does anything.
    let wrong = [
        Vec::new(),
        vec!["Authorization: Bearer wrong-token-0123456789".to_owned()],
        vec![format!("Authorization: Basic {CONTROL_TOKEN}")],
        vec!["Cookie: wallplay_control=wrong-token-0123456789".to_owned()],
        vec![format!("Cookie: other={CONTROL_TOKEN}")],
    ];
    for headers in &wrong {
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        for (method, path, body) in controller_requests() {
            let body = if body.is_null() {
                Vec::new()
            } else {
                body.to_string().into_bytes()
            };
            let (status, head, page) = request_with(address, method, path, &headers, &body);
            assert_eq!(status, 401, "{method} {path} with {headers:?}");
            assert!(head.contains("WWW-Authenticate: Bearer"), "{head}");
            assert!(!head.contains("Set-Cookie"), "{head}");
            let page = String::from_utf8_lossy(&page);
            if method == "GET" && path.starts_with("/?") {
                assert!(page.contains("Wrong token."), "{page}");
            } else if method == "GET" && ["/", "/calibration", "/login"].contains(&path) {
                assert!(page.contains(r#"action="/login""#), "{page}");
            } else {
                assert!(page.contains("control token"), "{method} {path}: {page}");
            }
        }
    }
    assert_eq!(poll(address, "left", 640, 360)["mode"], "idle");
    for name in ["manifest.json", "calibration.json", "layout.json"] {
        assert!(!state.path().join(name).exists(), "{name}");
    }

    // With the token as a bearer token or a cookie, the controls work.
    for headers in [[bearer.as_str()], [cookie.as_str()]] {
        let (status, _, body) = request_with(address, "GET", "/api/state", &headers, b"");
        assert_eq!(status, 200);
        let state: Value = serde_json::from_slice(&body).expect("state");
        assert_eq!(state["displays"][0]["id"], "left");
        let (status, _, page) = request_with(address, "GET", "/", &headers, b"");
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&page).contains("/api/state"));
    }
    // The token does not unlock pipeline descriptions, which could write
    // files, since it crosses plain HTTP.
    let launch = json!({"launch": "videotestsrc ! filesink location=x"}).to_string();
    let (status, _, _) = request_with(
        address,
        "POST",
        "/api/source",
        &[&bearer],
        launch.as_bytes(),
    );
    assert_eq!(status, 400);
    let (status, _, _) = request_with(address, "POST", "/api/patterns", &[&bearer], b"");
    assert_eq!(status, 200);
    assert_eq!(poll(address, "left", 640, 360)["mode"], "pattern");
    assert!(state.path().join("manifest.json").exists());
    assert_eq!(
        request(address, "GET", "/api/displays/left/pattern.svg", b"").0,
        200
    );
    let (status, _, _) = request_with(address, "POST", "/api/idle", &[&cookie], b"");
    assert_eq!(status, 200);
    assert_eq!(poll(address, "left", 640, 360)["mode"], "idle");
}

#[test]
fn signing_in_sets_the_control_cookie() {
    let state = tempfile::tempdir().expect("state directory");
    let token = ControlToken::new(CONTROL_TOKEN).expect("token");
    let (_app, address) = serve_with(state.path(), Duration::from_millis(200), Some(token));
    let form = ["Content-Type: application/x-www-form-urlencoded"];
    let (status, head, page) = request_with(
        address,
        "POST",
        "/login",
        &form,
        b"token=wrong-token-0123456789",
    );
    assert_eq!(status, 401);
    assert!(!head.contains("Set-Cookie"), "{head}");
    assert!(String::from_utf8_lossy(&page).contains("Wrong token."));

    // The form and a link with the token both sign in, and send the
    // browser to the control page without the token in its address.
    let body = format!("token={CONTROL_TOKEN}");
    let link = format!("/?token={CONTROL_TOKEN}");
    for (method, path, body) in [
        ("POST", "/login", body.as_bytes()),
        ("GET", link.as_str(), b"".as_slice()),
    ] {
        let (status, head, _) = request_with(address, method, path, &form, body);
        assert_eq!(status, 303, "{method} {path}: {head}");
        assert!(head.lines().any(|line| line == "Location: /"), "{head}");
        let cookie = head
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .expect("cookie");
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        let value = cookie.split(';').next().expect("cookie value");
        let (status, _, _) = request_with(
            address,
            "GET",
            "/api/state",
            &[&format!("Cookie: {value}")],
            b"",
        );
        assert_eq!(status, 200);
        // Signed in, the sign-in page leads to the control page.
        let (status, head, _) = request_with(
            address,
            "GET",
            "/login",
            &[&format!("Cookie: {value}")],
            b"",
        );
        assert_eq!(status, 303);
        assert!(head.lines().any(|line| line == "Location: /"), "{head}");
    }
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
    coded_frames(&pipeline, &app.measurement_log(), &[id], 3)
        .first()
        .map_or(0, BTreeMap::len)
}

#[test]
fn sources_end_fail_and_change_while_playing() {
    sources_end_fail_and_change_while_playing_with(Warp::Cpu);
}

#[test]
fn sources_end_fail_and_change_while_playing_on_the_gpu() {
    if gpu() {
        sources_end_fail_and_change_while_playing_with(Warp::Gpu);
    }
}

fn sources_end_fail_and_change_while_playing_with(warp: Warp) {
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
            ..options_with(warp)
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
    for (pts, (code, _)) in coded_frames(player.pipeline(), &measurement, &["left"], 5)
        .iter()
        .flatten()
    {
        assert_eq!(*code, frame_number(*pts, 30) % framecode::MODULUS);
    }
}

#[test]
fn layouts_change_while_playing() {
    layouts_change_while_playing_with(Warp::Cpu);
}

#[test]
fn layouts_change_while_playing_on_the_gpu() {
    if gpu() {
        layouts_change_while_playing_with(Warp::Gpu);
    }
}

fn layouts_change_while_playing_with(warp: Warp) {
    init();
    playing_on_a_new_layout(Fill::Contain, warp);
    playing_on_a_new_layout(Fill::Stretch, warp);
    // Cropping needs gst-plugins-good, which CI does not install.
    if gst::ElementFactory::find("videocrop").is_some() {
        playing_on_a_new_layout(Fill::Cover, warp);
    }
}

#[test]
fn covering_crops_follow_the_canvas() {
    covering_crops_follow_the_canvas_with(Warp::Cpu);
}

#[test]
fn covering_crops_follow_the_canvas_on_the_gpu() {
    if gpu() {
        covering_crops_follow_the_canvas_with(Warp::Gpu);
    }
}

fn covering_crops_follow_the_canvas_with(warp: Warp) {
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
    let mut player = Player::start(
        &two_displays(),
        &PlayerOptions {
            source: clip,
            fill: Fill::Cover,
            ..options_with(warp)
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
    *narrower
        .pointer_mut("/canvas/height")
        .expect("canvas height") = json!(300);
    let narrower = Layout::from_json(&narrower.to_string()).expect("layout");
    assert!(player.update_layout(&narrower, Fill::Cover));
    // And 300 of a 640x300 one.
    assert!(eventually(10, || rows(&crop) == (90, 90, 0, 0)));
}

#[test]
fn fills_bar_or_stretch_a_source_of_another_shape() {
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255; 4];
    init();
    // A white 4:3 source on the 16:9 canvas.
    let clip = Prepared::prepare(
        &Source::Launch(
            "videotestsrc pattern=white ! capsfilter caps=video/x-raw,width=640,height=480"
                .to_owned(),
        ),
        &Resolver::default(),
    )
    .expect("clip");
    let measurement = std::sync::Arc::default();
    let mut player = Player::start(
        &two_displays(),
        &PlayerOptions {
            source: clip,
            fill: Fill::Contain,
            ..options()
        },
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    // Shown whole, it has black bars left and right.
    assert!(eventually(10, || canvas_edge(&player, "input-source-1") == Some(BLACK)));
    // Stretched, it covers the canvas without cropping; the source starts
    // again for it.
    assert!(player.update_layout(&two_displays(), Fill::Stretch));
    assert!(eventually(10, || canvas_edge(&player, "input-source-2") == Some(WHITE)));
    assert!(player.pipeline().by_name("crop").is_none());
    // Cropping needs gst-plugins-good, which CI does not install.
    if gst::ElementFactory::find("videocrop").is_some() {
        // Cropped, it starts again with a crop.
        assert!(player.update_layout(&two_displays(), Fill::Cover));
        assert!(eventually(10, || canvas_edge(&player, "input-source-3") == Some(WHITE)));
        assert!(player.pipeline().by_name("crop").is_some());
    }
}

#[test]
fn layout_masks_reach_the_warps() {
    init();
    let mut masked: Value = serde_json::from_str(&two_displays_json()).expect("json");
    masked
        .pointer_mut("/screens/0")
        .and_then(Value::as_object_mut)
        .expect("left screen")
        .insert("mask".to_owned(), json!([[[0.5, 0], [1, 1], [0, 1]]]));
    let masked = Layout::from_json(&masked.to_string()).expect("layout");
    let measurement = std::sync::Arc::default();
    let mut player =
        Player::start(&masked, &options(), &measurement, Clock::new(), |_| {}).expect("player");
    let mask = |player: &Player, id: &str| {
        player
            .pipeline()
            .by_name(&format!("warp-{id}"))
            .expect("warp")
            .property::<String>("mask")
    };
    assert_eq!(mask(&player, "left"), "0.5,0 1,1 0,1");
    assert_eq!(mask(&player, "right"), "");
    // A mask belongs to the display: it changes with the layout at once.
    assert!(player.update_layout(&two_displays(), Fill::default()));
    assert_eq!(mask(&player, "left"), "");
}

/// The pixel at the middle of the left edge of the next canvas frame from
/// the input bin `bin`, if it is playing.
fn canvas_edge(player: &Player, bin: &str) -> Option<[u8; 4]> {
    let canvas = player
        .pipeline()
        .by_name(bin)?
        .downcast::<gst::Bin>()
        .ok()?
        .by_name("canvas")?;
    let pad = canvas.static_pad("src")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let pixel = (|| {
            let caps = pad.current_caps()?;
            let info_caps = gst_video::VideoInfo::from_caps(&caps).ok()?;
            let buffer = info.buffer()?;
            let frame =
                gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info_caps).ok()?;
            let stride = usize::try_from(*frame.plane_stride().first()?).ok()?;
            let row = usize::try_from(info_caps.height() / 2).ok()?;
            let data = frame.plane_data(0).ok()?;
            data.get(row * stride..row * stride + 4)?.try_into().ok()
        })();
        // The test stops waiting after a while; then nobody listens.
        let _ignored = sender.send(pixel);
        gst::PadProbeReturn::Remove
    });
    receiver.recv_timeout(Duration::from_secs(1)).ok().flatten()
}

fn playing_on_a_new_layout(fill: Fill, warp: Warp) {
    let layout = two_displays();
    let measurement = std::sync::Arc::default();
    // A source that is not live keeps playing through a new canvas size; its
    // own 4:3 shape is barred, stretched or cropped to each canvas.
    let clip = Prepared::prepare(
        &Source::Launch(
            "videotestsrc pattern=ball ! capsfilter caps=video/x-raw,width=640,height=480"
                .to_owned(),
        ),
        &Resolver::default(),
    )
    .expect("clip");
    let mut player = Player::start(
        &layout,
        &PlayerOptions {
            source: clip,
            fill,
            ..options_with(warp)
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
    for (pts, (code, size)) in coded_frames(player.pipeline(), &measurement, &["left", "right"], 5)
        .iter()
        .flatten()
    {
        assert_eq!(*code, frame_number(*pts, 30) % framecode::MODULUS);
        assert!(matches!((size.width, size.height), (320, 180) | (180, 320)));
    }
}

/// The size of the next frame from display `id`.
fn frame_size(player: &Player, id: &str) -> Size {
    let sink = player
        .pipeline()
        .by_name(&format!("sink-{id}"))
        .expect("sink");
    pull(&sink).2
}

fn preview_size(player: &Player, id: &str) -> Option<(u32, u32)> {
    let sample = player
        .preview_sink(id)?
        .property::<Option<gst::Sample>>("last-sample")?;
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    Some((info.width(), info.height()))
}

#[test]
fn stream_sizes_change_while_playing() {
    stream_sizes_change_while_playing_with(Warp::Cpu);
}

#[test]
fn stream_sizes_change_while_playing_on_the_gpu() {
    if gpu() {
        stream_sizes_change_while_playing_with(Warp::Gpu);
    }
}

fn stream_sizes_change_while_playing_with(warp: Warp) {
    init();
    let measurement = std::sync::Arc::default();
    let mut player = Player::start(
        &two_displays(),
        &options_with(warp),
        &measurement,
        Clock::new(),
        |_| {},
    )
    .expect("player");
    let pipeline = player.pipeline().clone();
    let left = pipeline.by_name("warp-left").expect("warp");
    let (factory, format) = match warp {
        Warp::Cpu => ("wallwarp", gst_video::VideoFormat::Rgba),
        Warp::Gpu => ("glwallwarp", gst_video::VideoFormat::I420),
    };
    assert_eq!(left.factory().expect("factory").name(), factory);
    let stream = pipeline.by_name("sink-left").expect("sink");
    let sample = stream
        .emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::from_seconds(5)])
        .expect("sample");
    let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
    assert_eq!(info.format(), format);
    let new_size = Size {
        width: 240,
        height: 136,
    };
    assert_eq!(
        frame_size(&player, "left"),
        Size {
            width: 320,
            height: 180
        }
    );
    let mut resized = two_displays();
    resized.screens.first_mut().expect("left").output = new_size;
    assert_ne!(resized, two_displays());
    assert!(player.update_layout(&resized, Fill::default()));
    let route = player.route("left").expect("left");
    assert_eq!(route.output, new_size);
    assert_eq!(route.code, CodeGeometry::for_output(240, 136));
    assert!(eventually(10, || frame_size(&player, "left") == new_size));
    assert!(eventually(10, || preview_size(&player, "left") == Some((240, 136))));
    // The same elements play on, and the other display is untouched.
    assert_eq!(pipeline.by_name("warp-left").as_ref(), Some(&left));
    assert_eq!(player.source_status().restarts, 0);
    assert_eq!(
        frame_size(&player, "right"),
        Size {
            width: 180,
            height: 320
        }
    );
    // Frame codes follow the new size.
    player.set_measuring(true);
    for (pts, (code, size)) in coded_frames(player.pipeline(), &measurement, &["left", "right"], 5)
        .iter()
        .flatten()
    {
        assert_eq!(*code, frame_number(*pts, 30) % framecode::MODULUS);
        assert!(*size == new_size || size.width == 180);
    }
}

#[test]
fn stream_sizes_change_under_running_frames() {
    stream_sizes_change_under_running_frames_with(Warp::Cpu);
}

#[test]
fn stream_sizes_change_under_running_frames_on_the_gpu() {
    if gpu() {
        stream_sizes_change_under_running_frames_with(Warp::Gpu);
    }
}

fn stream_sizes_change_under_running_frames_with(warp: Warp) {
    init();
    let measurement = std::sync::Arc::default();
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let reported = std::sync::Arc::clone(&failures);
    let mut player = Player::start(
        &two_displays(),
        &options_with(warp),
        &measurement,
        Clock::new(),
        move |event| {
            if let player::PlayerEvent::Failed(error) = event {
                reported.lock().expect("failures").push(error);
            }
        },
    )
    .expect("player");
    assert_eq!(frame_size(&player, "left").width, 320);
    // Each change meets frames of the previous size already on their way.
    let mut layout = two_displays();
    let mut size = Size {
        width: 320,
        height: 180,
    };
    for step in 0..60 {
        size = Size {
            width: 240 + 8 * (step % 2),
            height: 136,
        };
        layout.screens.first_mut().expect("left").output = size;
        assert!(player.update_layout(&layout, Fill::default()));
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(
        eventually(10, || frame_size(&player, "left") == size),
        "frames stopped"
    );
    assert_eq!(*failures.lock().expect("failures"), Vec::<String>::new());
}

/// `two_displays()` with a third display, or without "right".
fn with_third() -> Layout {
    let mut layout = two_displays();
    let mut third = layout.screens.first().expect("left").clone();
    "third".clone_into(&mut third.id);
    layout.screens.insert(1, third);
    layout
}

fn without_right() -> Layout {
    let mut layout = two_displays();
    layout.screens.retain(|screen| screen.id != "right");
    layout
}

#[test]
fn displays_come_and_go_while_playing() {
    displays_come_and_go_while_playing_with(Warp::Cpu);
}

#[test]
fn displays_come_and_go_while_playing_on_the_gpu() {
    if gpu() {
        displays_come_and_go_while_playing_with(Warp::Gpu);
    }
}

fn displays_come_and_go_while_playing_with(warp: Warp) {
    init();
    for audio in [false, true] {
        let measurement = std::sync::Arc::default();
        let mut player = Player::start(
            &two_displays(),
            &PlayerOptions {
                audio,
                ..options_with(warp)
            },
            &measurement,
            Clock::new(),
            |_| {},
        )
        .expect("player");
        let pipeline = player.pipeline().clone();
        let left = pipeline.by_name("sink-left").expect("sink");
        assert!(eventually(10, || player.pipeline().current_state()
            == gst::State::Playing));
        let hears = |id: &str| {
            let sink = pipeline.by_name(&format!("sound-{id}")).expect("sound");
            sink.emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(5)],
            )
            .is_some()
        };

        // A third display joins, between the two in layout order.
        assert!(player.update_layout(&with_third(), Fill::default()));
        let order: Vec<_> = player.routes().iter().map(|route| &route.display).collect();
        assert_eq!(order, ["left", "third", "right"]);
        let route = player.route("third").expect("third");
        assert_eq!(route.output, player.route("left").expect("left").output);
        assert_eq!(
            route.generation,
            player.route("left").expect("left").generation
        );
        assert_eq!(frame_size(&player, "third").width, 320);
        assert!(eventually(10, || preview_size(&player, "third").is_some()));
        assert!(!audio || hears("third"));

        // One leaves; the rest play on.
        assert!(player.update_layout(&without_right(), Fill::default()));
        assert!(player.route("right").is_none());
        assert!(pipeline.by_name("sink-right").is_none());
        assert!(pipeline.by_name("warp-right").is_none());
        assert!(pipeline.by_name("sound-right").is_none());
        assert!(player.route("third").is_none());
        assert_eq!(frame_size(&player, "left").width, 320);

        // It can come back at once, as its old elements are out of the way.
        assert!(player.update_layout(&two_displays(), Fill::default()));
        assert_eq!(frame_size(&player, "right").height, 320);
        assert!(!audio || hears("right"));
        assert_eq!(pipeline.by_name("sink-left").as_ref(), Some(&left));
        // The new branches report nothing wrong, and frames carry codes.
        player.set_measuring(true);
        coded_frames(player.pipeline(), &measurement, &["left", "right"], 3);

        // No displays at all needs a new pipeline.
        let mut empty = two_displays();
        empty.screens.clear();
        assert!(!player.update_layout(&empty, Fill::default()));
        assert!(player.route("left").is_some());
    }
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
    // Cameras and screens must exist; the reason says what is missing.
    let (code, error) = json_request(
        address,
        "POST",
        "/api/source",
        &json!({"camera": "No such camera"}),
    );
    assert_eq!(code, 422, "{error}");
    assert!(error.to_string().contains("no camera"), "{error}");
    let (code, error) = json_request(address, "POST", "/api/source", &json!({"screen": 0}));
    assert_eq!(code, 422, "{error}");
    let expected = if wallplay::capture::SCREEN_CAPTURE {
        "numbered from 1"
    } else {
        "macOS only"
    };
    assert!(error.to_string().contains(expected), "{error}");
    assert_eq!(status("/api/source", json!({"camera": ""})), 422);
    let devices = ok(address, "GET", "/api/devices", &Value::Null);
    assert!(devices["devices"].is_array(), "{devices}");
    assert!(devices["screen_capture"].is_boolean(), "{devices}");
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
    // the aligner's settling time, and the left keeps a 20 ms jitter buffer
    // of its own.
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
        let report = json!({"rtp": frames, "rtp_timestamps": true, "jitter_buffer": 20.0});
        let path = format!("/api/displays/{id}/frames");
        assert_eq!(json_request(address, "POST", &path, &report).0, 204);
    }
    // The left display gets most of the 50 ms in one step, on top of the
    // buffer it kept.
    assert!(eventually(10, || {
        poll(address, "left", 480, 270)["buffer_ms"]
            .as_u64()
            .is_some_and(|ms| (50..=60).contains(&ms))
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

/// The warnings the control page shows for display `id`.
fn warnings(address: SocketAddr, id: &str) -> Vec<String> {
    let state = ok(address, "GET", "/api/state", &Value::Null);
    state
        .get("displays")
        .and_then(Value::as_array)
        .expect("displays")
        .iter()
        .find(|display| display.get("id") == Some(&json!(id)))
        .and_then(|display| serde_json::from_value(display.get("warnings")?.clone()).ok())
        .expect("warnings")
}

#[test]
fn displays_that_present_nothing_are_warned_about() {
    let state = tempfile::tempdir().expect("state directory");
    let (app, address) = serve(state.path());
    calibrate(address);
    ok(
        address,
        "POST",
        "/api/layout",
        &json!({"mode": "viewpoint", "canvas": [1280, 720]}),
    );
    ok(address, "POST", "/api/play", &Value::Null);
    // Pages that say they play their streams, the left one hidden.
    let page = |id: &str, size: &str, hidden: bool| {
        let path = format!(
            "/api/displays/{id}/poll?since=0&w={}&scale=2&fullscreen=1&player=playing&hidden={}",
            size.replace('x', "&h="),
            u8::from(hidden)
        );
        ok(address, "GET", &path, &Value::Null);
    };
    page("left", "480x270", true);
    page("right", "270x480", false);
    // A hidden page says so, measuring or not.
    let hidden = "page hidden: its browser draws no frames".to_owned();
    assert!(warnings(address, "left").contains(&hidden));
    let covered = "reported no frames for 5 s: is its window covered?".to_owned();
    ok(address, "POST", "/api/measure", &json!({"on": true}));
    // Displays get a few seconds to start reporting.
    assert!(!warnings(address, "right").contains(&covered));
    assert!(eventually(10, || {
        page("right", "270x480", false);
        warnings(address, "right").contains(&covered)
    }));
    assert_eq!(warnings(address, "left"), std::slice::from_ref(&hidden));
    // Frames presented now clear the warning.
    let sink = app
        .pipeline()
        .and_then(|pipeline| pipeline.by_name("sink-right"))
        .expect("sink");
    let frame = frame_number(pull(&sink).0, 30);
    let now = ok(address, "POST", "/api/clock", &Value::Null)["now"]
        .as_f64()
        .expect("now");
    let path = "/api/displays/right/frames";
    let report = json!({"rtp": [[(frame * 3000) % (1 << 32), now, null]], "rtp_timestamps": true});
    assert_eq!(json_request(address, "POST", path, &report).0, 204);
    assert_eq!(warnings(address, "right"), Vec::<String>::new());
    // Reports of frames it cannot tell apart say that instead, once the
    // frame above is 5 s old.
    let unidentified = "reports frames it cannot identify".to_owned();
    assert!(eventually(10, || {
        page("right", "270x480", false);
        let report = json!({"unreadable": 30, "rtp_timestamps": false});
        assert_eq!(json_request(address, "POST", path, &report).0, 204);
        warnings(address, "right") == [unidentified.clone()]
    }));
    // A page that is not playing its stream is not asked about its window.
    let path =
        "/api/displays/right/poll?since=0&w=270&h=480&scale=2&fullscreen=1&player=connecting";
    ok(address, "GET", path, &Value::Null);
    assert_eq!(warnings(address, "right"), Vec::<String>::new());
    // Not measuring, nothing is expected.
    ok(address, "POST", "/api/measure", &json!({"on": false}));
    page("left", "480x270", false);
    page("right", "270x480", false);
    assert_eq!(warnings(address, "left"), Vec::<String>::new());
    assert_eq!(warnings(address, "right"), Vec::<String>::new());
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
fn new_layouts_keep_the_displays_masks() {
    let state = tempfile::tempdir().expect("state directory");
    let viewpoint = json!({"mode": "viewpoint", "canvas": {"longest": 1000}});
    {
        let (_app, address) = serve(state.path());
        calibrate(address);
        ok(address, "POST", "/api/layout", &viewpoint);
    }
    // A mask written into the stored layout by hand...
    let path = state.path().join("layout.json");
    let mut stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("layout")).expect("JSON");
    let screens = stored
        .get_mut("screens")
        .and_then(Value::as_array_mut)
        .expect("screens");
    for screen in screens.iter_mut() {
        if screen.get("id") == Some(&json!("left"))
            && let Some(screen) = screen.as_object_mut()
        {
            screen.insert("mask".to_owned(), json!([[[0.5, 0], [1, 1], [0, 1]]]));
        }
    }
    std::fs::write(&path, stored.to_string()).expect("writing the layout");
    // ...is played by a restarted server and kept by its next layout.
    let (_app, address) = serve(state.path());
    ok(address, "POST", "/api/layout", &viewpoint);
    let layout = Layout::from_json(
        &String::from_utf8(request(address, "GET", "/api/layout", b"").1).expect("UTF-8"),
    )
    .expect("layout");
    let mask = |id: &str| layout.screen(id).expect("screen").mask.to_string();
    assert_eq!(mask("left"), "0.5,0 1,1 0,1");
    assert_eq!(mask("right"), "");
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
    // display 12 ms later than the left. Each display's leaky queue drops
    // frames of its own when its warp falls behind, as under load, so pull
    // until enough frames reached both, keeping to the last 15 s, well inside
    // the measurement's window.
    let pipeline = app.pipeline().expect("pipeline");
    let (mut left, mut right) = (Codes::new(), Codes::new());
    // Frames both displays delivered, once each: as the source takes over
    // from the standby, one frame can come with two running times.
    let in_both = |left: &Codes, right: &Codes| {
        let mut both: Vec<_> = left
            .keys()
            .filter(|pts| right.contains_key(pts))
            .copied()
            .collect();
        both.dedup_by_key(|pts| frame_number(*pts, 30));
        both
    };
    for _ in 0..20 {
        let [more_left, more_right] = <[Codes; 2]>::try_from(coded_frames(
            &pipeline,
            &app.measurement_log(),
            &["left", "right"],
            10,
        ))
        .expect("two displays");
        left.extend(more_left);
        right.extend(more_right);
        if let Some(newest) = left.keys().chain(right.keys()).max().copied() {
            let oldest = newest.saturating_sub(gst::ClockTime::from_seconds(15));
            left.retain(|pts, _| *pts >= oldest);
            right.retain(|pts, _| *pts >= oldest);
        }
        if in_both(&left, &right).len() >= 3 {
            break;
        }
    }
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
    let common = in_both(&left, &right);
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
