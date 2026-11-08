//! The demo server's state and routes.
//!
//! Displays open `/display/<id>` and long-poll `/api/displays/<id>/poll`,
//! which carries their viewport to the server and brings back what to show:
//! nothing, their calibration pattern, or their stream. The control page at
//! `/` drives the workflow through the JSON API below. Calibration files are
//! kept in the state directory under the names `wallcalibrate` uses, so they
//! survive restarts and work with its command line.
//!
//! Lock order: `state`, then `measurement`. Pipeline threads take only
//! `measurement` and `player_status`. Players are stopped without holding
//! `state`, since stopping one can take seconds, or forever.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wall_geometry::{Layout, Size, is_valid_screen_id};
use wallcalibrate::calibration::{self, Calibration, SolveOptions, Status};
use wallcalibrate::canvas::{self, CanvasSize, LayoutOptions, Mode};
use wallcalibrate::detect::{self, Detections, Photo};
use wallcalibrate::manifest::{Manifest, Placement};
use wallcalibrate::{pattern, preview, tag};

use crate::capture;
use crate::drift::{FrameReport, Measurement};
use crate::framecode::CodeGeometry;
use crate::http::{Handler, Request, Response};
use crate::player::{self, Clock, Endpoint, Fill, Player, PlayerEvent, PlayerOptions};
use crate::source::{self, Prepared, Source};
use crate::sync::Aligner;

const CONTROL_PAGE: &str = include_str!("control.html");
const DISPLAY_PAGE: &str = include_str!("display.html");

/// A display counts as online this long after its last poll ended.
const ONLINE_GRACE: Duration = Duration::from_secs(6);

/// Largest relative aspect ratio difference treated as a match.
const ASPECT_TOLERANCE: f64 = 0.01;

/// How long a stopping pipeline may take before playback moves on without it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// While recording, a display whose page plays its stream but reports no
/// frame for this long, in milliseconds, while the pipeline sends it frames,
/// is warned about: browsers may draw no frames for a covered window.
const NO_FRAMES_MS: f64 = 5000.0;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where calibration files and the photo are kept.
    pub state_dir: PathBuf,
    pub player: PlayerOptions,
    /// How long a display's poll waits for something to change.
    pub poll_timeout: Duration,
    /// Longest side of a display's stream; larger viewports are scaled down.
    pub max_output: u32,
    /// The display that plays the sound at first; without one, the streams
    /// carry none.
    pub sound: Option<String>,
}

/// A display's browser viewport, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    /// Device pixels per CSS pixel.
    pub scale: f64,
    pub fullscreen: bool,
}

impl Viewport {
    fn from_query(request: &Request) -> Option<Self> {
        let number = |name| {
            request
                .query(name)?
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 0)
        };
        Some(Self {
            width: number("w")?,
            height: number("h")?,
            scale: request
                .query("scale")
                .and_then(|scale| scale.parse().ok())
                .filter(|scale: &f64| scale.is_finite() && *scale > 0.0)
                .unwrap_or(1.0),
            fullscreen: request.query("fullscreen") == Some("1"),
        })
    }

    /// The stream size for this viewport: its aspect ratio, at most `max`
    /// pixels on the longer side, and even, as video encoders require.
    #[must_use]
    pub fn output(&self, max: u32) -> Size {
        let (width, height) = (f64::from(self.width), f64::from(self.height));
        let scale = (f64::from(max) / width.max(height)).min(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is positive and at most `max`"
        )]
        let even = |value: f64| ((value * scale / 2.0).round() as u32 * 2).max(2);
        Size {
            width: even(width),
            height: even(height),
        }
    }
}

fn aspect_differs(viewport: &Viewport, output: Size) -> bool {
    let ratio = f64::from(viewport.width) * f64::from(output.height)
        / (f64::from(viewport.height) * f64::from(output.width));
    (ratio - 1.0).abs() > ASPECT_TOLERANCE
}

struct Display {
    viewport: Option<Viewport>,
    agent: String,
    address: String,
    /// What the page reports about its stream.
    player: String,
    /// Whether the page reports itself hidden, so its browser draws nothing.
    hidden: bool,
    polls: usize,
    last_seen: Instant,
}

impl Display {
    fn online(&self) -> bool {
        self.polls > 0 || self.last_seen.elapsed() < ONLINE_GRACE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Show {
    Idle,
    Patterns,
    Play,
}

struct GeneratedLayout {
    layout: Layout,
    omitted: Vec<String>,
    units_per_pixel: Option<f64>,
    serial: u64,
    /// The options it was made with, unknown for a layout loaded from disk.
    request: Option<LayoutRequest>,
}

struct State {
    /// Bumped whenever any display's command may have changed.
    version: u64,
    displays: BTreeMap<String, Display>,
    manifest: Option<Manifest>,
    manifest_serial: u64,
    calibration: Option<Calibration>,
    calibration_serial: u64,
    /// The photo as a reduced JPEG for the editor and the simulation, made
    /// when first needed.
    photo_jpeg: Option<Arc<Vec<u8>>>,
    layout: Option<GeneratedLayout>,
    show: Show,
    player: Option<Player>,
    /// What plays; the command line's choice until the control page changes it.
    source: Prepared,
    /// The pattern shown whenever the source is not.
    standby: String,
    /// Whether a source that ends starts again.
    restart: bool,
    /// The browsers' jitter buffer target; `None` leaves their default.
    buffer_ms: Option<u32>,
    /// The display that plays the sound. Every stream carries it, the other
    /// displays muted, so lip sync holds every display's video back alike;
    /// without one, the streams carry none.
    sound: Option<String>,
    /// How the source fills the canvas, chosen with the layout.
    fill: Fill,
    measuring: bool,
    /// Delays aligning the displays, while aligning.
    aligner: Option<Aligner>,
    /// When recording last started, on the server's clock.
    recording_since: Option<f64>,
}

impl State {
    /// Whether the pipeline records frames and the browsers report them.
    fn recording(&self) -> bool {
        self.measuring || self.aligner.is_some()
    }

    /// Note when recording started, after measuring or aligning changed.
    fn note_recording(&mut self, now: f64) {
        self.recording_since = if self.recording() {
            self.recording_since.or(Some(now))
        } else {
            None
        };
    }

    /// Display `id`'s jitter buffer target: the chosen one, or while
    /// aligning, its delay on top of the larger of the chosen one and the
    /// buffer it kept before the delay.
    fn buffer_ms(&self, id: &str) -> Option<u32> {
        let Some(aligner) = &self.aligner else {
            return self.buffer_ms;
        };
        let delay = aligner.delay(id);
        if delay < crate::sync::MIN_DELAY_MS {
            return self.buffer_ms;
        }
        let base = f64::from(self.buffer_ms.unwrap_or(0)).max(aligner.base(id));
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the target is positive, and clamped below"
        )]
        let target = (base + delay).round().min(f64::from(MAX_BUFFER_MS)) as u32;
        Some(target)
    }

    /// The aligner's progress and each display's buffer target, while
    /// aligning.
    fn aligning(&self) -> Value {
        let Some(aligner) = &self.aligner else {
            return Value::Null;
        };
        let buffers: BTreeMap<&String, Option<u32>> = aligner
            .delays()
            .keys()
            .map(|id| (id, self.buffer_ms(id)))
            .collect();
        json!({
            "steps": aligner.steps,
            "spread_ms": aligner.spread_ms,
            "aligned": aligner.aligned(),
            "delays_ms": aligner.delays(),
            "buffer_ms": buffers,
            "tolerance_ms": crate::sync::TOLERANCE_MS,
        })
    }

    /// What display `id` should show.
    fn command(&self, id: &str) -> Command {
        let idle = |message: &str| Command::Idle {
            message: message.to_owned(),
        };
        match self.show {
            Show::Idle => idle("Waiting for the control page."),
            Show::Patterns => match self
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.screen(id))
            {
                Some(screen) => Command::Pattern {
                    pattern: format!("/api/displays/{id}/pattern.svg?v={}", self.manifest_serial),
                    output: screen.output,
                },
                None => {
                    idle("Not part of this calibration; show the patterns again to include it.")
                }
            },
            Show::Play => match self.player.as_ref().and_then(|player| player.route(id)) {
                Some(route) => Command::Play {
                    endpoint: route.endpoint.clone(),
                    generation: route.generation,
                    output: route.output,
                    code: route.code,
                    measuring: self.recording(),
                    buffer_ms: self.buffer_ms(id),
                    sound: self.sound.as_deref() == Some(id),
                },
                None => idle("Not part of the layout."),
            },
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct PlayerStatus {
    /// Which start this status belongs to; events of stopped players, which
    /// may report errors while stopping, are ignored.
    #[serde(skip)]
    run: u64,
    state: String,
    error: Option<String>,
}

/// What one display should show.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum Command {
    Idle {
        message: String,
    },
    Pattern {
        pattern: String,
        output: Size,
    },
    Play {
        endpoint: Endpoint,
        generation: u32,
        output: Size,
        code: Option<CodeGeometry>,
        measuring: bool,
        buffer_ms: Option<u32>,
        /// Whether this display plays the stream's sound.
        sound: bool,
    },
}

/// A failed request: an HTTP status and a message for the user.
#[derive(Debug)]
struct Failure {
    status: u16,
    message: String,
}

impl Failure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(422, format!("{error:#}"))
    }
}

type Reply = Result<Response, Failure>;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum LayoutMode {
    #[default]
    Viewpoint,
    Planar,
}

/// The canvas: `[width, height]` with the displays centered in it, or
/// `{"longest": pixels}` for exactly the displays' extent.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum CanvasRequest {
    Fit((u32, u32)),
    Tight { longest: u32 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LayoutRequest {
    #[serde(default)]
    mode: LayoutMode,
    /// The display the layout aligns to: the reference plane in planar mode,
    /// the display leveled in viewpoint mode.
    #[serde(alias = "reference")]
    anchor: Option<String>,
    /// Measured width and height of the anchor's picture, in planar mode.
    reference_size: Option<(f64, f64)>,
    canvas: CanvasRequest,
    /// Make every display an upright rectangle of its aspect ratio.
    #[serde(default)]
    straighten: bool,
    #[serde(default)]
    allow_incomplete: bool,
    /// How a video of another shape fills the canvas.
    #[serde(default)]
    fill: Fill,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasureRequest {
    on: bool,
}

/// How the source plays; fields left out stay as they are.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybackRequest {
    restart: Option<bool>,
    standby: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BufferRequest {
    /// Jitter buffer target in milliseconds, or `null` for the default.
    ms: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoundRequest {
    /// The display that plays the sound, or `null` for none.
    display: Option<String>,
}

/// Largest jitter buffer target browsers accept.
const MAX_BUFFER_MS: u32 = 4000;

/// How often the measurement is checked, to align displays and to decide
/// whether frame codes are drawn.
const TEND_INTERVAL: Duration = Duration::from_millis(500);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A value no other run of the server, nor another app in this process,
/// has used.
fn new_epoch() -> String {
    static APPS: AtomicU64 = AtomicU64::new(0);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!("{started}-{}", APPS.fetch_add(1, Ordering::Relaxed))
}

/// Read a state file if it exists.
fn read_state_file(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// The extension for a photo, from its first bytes.
fn photo_extension(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xff, 0xd8, 0xff, ..] => "jpg",
        [0x89, b'P', b'N', b'G', ..] => "png",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "heic",
        _ => "img",
    }
}

const PHOTO_EXTENSIONS: [&str; 4] = ["jpg", "png", "heic", "img"];

pub struct App {
    config: Config,
    clock: Clock,
    state: Mutex<State>,
    changed: Condvar,
    measurement: Arc<Mutex<Measurement>>,
    player_status: Arc<Mutex<PlayerStatus>>,
    /// Pipelines that did not stop in time; see [`Player::stop`].
    stuck_pipelines: AtomicUsize,
    /// Players started so far, numbering their statuses.
    runs: AtomicU64,
    /// Tells this run of the server from earlier ones, whose state versions
    /// a display page may still hold.
    epoch: String,
}

impl App {
    /// Start with the calibration files already in the state directory.
    pub fn new(config: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("creating {}", config.state_dir.display()))?;
        let file = |name: &str| read_state_file(&config.state_dir.join(name));
        let manifest = file("manifest.json")?
            .map(|json| Manifest::from_json(&json))
            .transpose()
            .context("loading manifest.json")?;
        let calibration = file("calibration.json")?
            .map(|json| Calibration::from_json(&json))
            .transpose()
            .context("loading calibration.json")?;
        let layout = file("layout.json")?
            .map(|json| Layout::from_json(&json))
            .transpose()
            .context("loading layout.json")?
            .map(|layout| GeneratedLayout {
                layout,
                omitted: Vec::new(),
                units_per_pixel: None,
                serial: 1,
                request: None,
            });
        let (source, standby, restart, sound) = (
            config.player.source.clone(),
            config.player.standby.clone(),
            config.player.restart,
            config.sound.clone(),
        );
        let app = Arc::new(Self {
            config,
            clock: Clock::new(),
            state: Mutex::new(State {
                version: 1,
                displays: BTreeMap::new(),
                manifest,
                manifest_serial: 1,
                calibration,
                calibration_serial: 1,
                photo_jpeg: None,
                layout,
                show: Show::Idle,
                player: None,
                source,
                standby,
                restart,
                buffer_ms: None,
                sound,
                fill: Fill::default(),
                measuring: false,
                aligner: None,
                recording_since: None,
            }),
            changed: Condvar::new(),
            measurement: Arc::default(),
            player_status: Arc::default(),
            stuck_pipelines: AtomicUsize::new(0),
            runs: AtomicU64::new(0),
            epoch: new_epoch(),
        });
        let tending = Arc::downgrade(&app);
        std::thread::Builder::new()
            .name("wallplay-align".to_owned())
            .spawn(move || {
                // Ends with the app.
                while let Some(app) = tending.upgrade() {
                    app.tend_measurement();
                    drop(app);
                    std::thread::sleep(TEND_INTERVAL);
                }
            })
            .context("starting the aligning thread")?;
        Ok(app)
    }

    /// The running pipeline, if playing, for tests and diagnostics.
    #[must_use]
    pub fn pipeline(&self) -> Option<gst::Pipeline> {
        self.state()
            .player
            .as_ref()
            .map(|player| player.pipeline().clone())
    }

    /// What the pipeline and browsers recorded while measuring, for tests
    /// and diagnostics.
    #[must_use]
    pub fn measurement_log(&self) -> Arc<Mutex<Measurement>> {
        Arc::clone(&self.measurement)
    }

    /// Answer requests with this app.
    #[must_use]
    pub fn handler(self: &Arc<Self>) -> Handler {
        let app = Arc::clone(self);
        Arc::new(move |request| app.handle(&request))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Wake every display's poll after a change.
    fn bump(&self, state: &mut State) {
        state.version += 1;
        self.changed.notify_all();
    }

    fn save(&self, name: &str, contents: &[u8]) -> Result<()> {
        let path = self.config.state_dir.join(name);
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
    }

    pub fn handle(self: &Arc<Self>, request: &Request) -> Response {
        let path = request.path.trim_matches('/');
        let segments: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').collect()
        };
        let reply = match (request.method.as_str(), segments.as_slice()) {
            ("GET", []) => Ok(Response::html(CONTROL_PAGE)),
            ("GET", ["display"]) => Ok(Response::html(DISPLAY_PAGE.replace("{{ID}}", ""))),
            ("GET", ["display", id]) => {
                valid_id(id).map(|id| Response::html(DISPLAY_PAGE.replace("{{ID}}", id)))
            }
            ("GET", ["calibration"]) => self.editor(),
            ("GET", ["api", "state"]) => Ok(self.status()),
            ("POST", ["api", "clock"]) => {
                Ok(Response::json(&json!({ "now": self.clock.now_ms() })))
            }
            ("GET", ["api", "displays", id, "poll"]) => {
                valid_id(id).map(|id| self.poll(id, request))
            }
            ("POST", ["api", "displays", id, "frames"]) => {
                valid_id(id).and_then(|id| self.frames(id, request))
            }
            ("GET", ["api", "displays", id, "pattern.svg"]) => {
                valid_id(id).and_then(|id| self.pattern(id))
            }
            ("DELETE", ["api", "displays", id]) => valid_id(id).and_then(|id| self.forget(id)),
            ("POST", ["api", "patterns"]) => self.show_patterns(),
            ("GET", ["api", "manifest"]) => self.file("manifest.json"),
            ("POST", ["api", "photo"]) => self.upload_photo(&request.body),
            ("GET", ["api", "photo.jpg"]) => self.photo(),
            ("GET", ["api", "displays", id, "preview.jpg"]) => {
                valid_id(id).and_then(|id| self.preview(id))
            }
            ("GET", ["api", "calibration"]) => self.file("calibration.json"),
            ("PUT", ["api", "calibration"]) => self.put_calibration(&request.body),
            ("POST", ["api", "layout"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|options| self.make_layout(&options)),
            ("GET", ["api", "layout"]) => self.file("layout.json"),
            ("GET", ["api", "layout.svg"]) => self.layout_svg(),
            ("POST", ["api", "play"]) => self.play(),
            // Each connection has its own thread, so finding the devices,
            // which takes a moment, holds up nothing else.
            ("GET", ["api", "devices"]) => Ok(Response::json(&json!({
                "devices": capture::devices(),
                "screen_capture": capture::SCREEN_CAPTURE,
            }))),
            ("POST", ["api", "source"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|source| self.set_source(&source)),
            ("POST", ["api", "playback"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|playback| self.set_playback(&playback)),
            ("POST", ["api", "buffer"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|buffer: BufferRequest| self.set_buffer(buffer.ms)),
            ("POST", ["api", "sound"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|sound: SoundRequest| self.set_sound(sound.display)),
            ("POST", ["api", "idle"]) => Ok(self.idle()),
            ("POST", ["api", "measure"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|measure: MeasureRequest| self.measure(measure.on)),
            ("GET", ["api", "measure"]) => Ok(self.measurement()),
            ("POST", ["api", "align"]) => request
                .json()
                .map_err(|error| Failure::new(400, format!("{error:#}")))
                .and_then(|align: MeasureRequest| self.align(align.on)),
            _ => Err(Failure::new(
                404,
                format!("no route for {} {}", request.method, request.path),
            )),
        };
        reply.unwrap_or_else(|failure| Response::error(failure.status, &failure.message))
    }

    fn file(&self, name: &str) -> Reply {
        let path = self.config.state_dir.join(name);
        read_state_file(&path)?
            .map(|text| Response::new(200, "application/json", text))
            .ok_or_else(|| Failure::new(404, format!("there is no {name} yet")))
    }

    fn poll(&self, id: &str, request: &Request) -> Response {
        let since: u64 = request
            .query("since")
            .and_then(|since| since.parse().ok())
            .unwrap_or(0);
        let mut state = self.state();
        let display = state
            .displays
            .entry(id.to_owned())
            .or_insert_with(|| Display {
                viewport: None,
                agent: String::new(),
                address: String::new(),
                player: String::new(),
                hidden: false,
                polls: 0,
                last_seen: Instant::now(),
            });
        if let Some(viewport) = Viewport::from_query(request) {
            display.viewport = Some(viewport);
        }
        if let Some(player) = request.query("player") {
            display.player = player.chars().take(200).collect();
        }
        display.hidden = request.query("hidden") == Some("1");
        request
            .header("user-agent")
            .unwrap_or_default()
            .clone_into(&mut display.agent);
        display.address = request
            .peer
            .map(|peer| peer.ip().to_string())
            .unwrap_or_default();
        display.polls += 1;
        // A page that last heard from an earlier run of the server holds
        // that run's version, which this one may not reach for a while.
        // Pages from before epochs send none, and are only caught when their
        // version is ahead of this run's; they would otherwise poll in a loop.
        let current = match request.query("epoch") {
            Some(epoch) => epoch == self.epoch,
            None => since <= state.version,
        };
        let deadline = Instant::now() + self.config.poll_timeout;
        while current && state.version <= since {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = match self.changed.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        if let Some(display) = state.displays.get_mut(id) {
            display.polls = display.polls.saturating_sub(1);
            display.last_seen = Instant::now();
        }
        let command = state.command(id);
        Response::json(&json!({
            "epoch": self.epoch,
            "version": state.version,
            "command": command,
        }))
    }

    fn frames(&self, id: &str, request: &Request) -> Reply {
        let report: FrameReport = request
            .json()
            .map_err(|error| Failure::new(400, format!("{error:#}")))?;
        let mut measurement = lock(&self.measurement);
        measurement.reported(id, self.clock.now_ms());
        measurement.shown(id, &report, self.config.player.fps);
        Ok(Response::no_content())
    }

    fn pattern(&self, id: &str) -> Reply {
        let state = self.state();
        let screen = state
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.screen(id))
            .ok_or_else(|| Failure::new(404, format!("{id} has no pattern")))?;
        Ok(Response::new(200, "image/svg+xml", pattern::svg(screen)?))
    }

    fn forget(&self, id: &str) -> Reply {
        let mut state = self.state();
        match state.displays.get(id) {
            None => Err(Failure::new(404, format!("no display {id}"))),
            Some(display) if display.online() => Err(Failure::new(
                409,
                format!("{id} is online; close its page first"),
            )),
            Some(_) => {
                state.displays.remove(id);
                self.bump(&mut state);
                Ok(Response::no_content())
            }
        }
    }

    /// Give every online display markers sized to its viewport, and show them.
    fn show_patterns(&self) -> Reply {
        let mut state = self.state();
        let screens: Vec<(String, Size)> = state
            .displays
            .iter()
            .filter(|(_, display)| display.online())
            .filter_map(|(id, display)| {
                Some((id.clone(), display.viewport?.output(self.config.max_output)))
            })
            .collect();
        if screens.is_empty() {
            return Err(Failure::new(
                409,
                "no display is connected; open /display/<id> on every display first",
            ));
        }
        let manifest = Manifest::generate(&screens, 0, Placement::default())?;
        self.save("manifest.json", manifest.to_json()?.as_bytes())?;
        state.manifest = Some(manifest);
        state.manifest_serial += 1;
        let player = state.player.take();
        *lock(&self.player_status) = PlayerStatus::default();
        state.measuring = false;
        state.aligner = None;
        state.recording_since = None;
        state.show = Show::Patterns;
        self.bump(&mut state);
        drop(state);
        self.retire(player);
        Ok(Response::json(
            &json!({ "displays": screens.iter().map(|(id, _)| id).collect::<Vec<_>>() }),
        ))
    }

    /// Detect markers in an uploaded photo and fit every display.
    fn upload_photo(&self, body: &[u8]) -> Reply {
        if body.is_empty() {
            return Err(Failure::new(400, "the photo is empty"));
        }
        let manifest = self
            .state()
            .manifest
            .clone()
            .ok_or_else(|| Failure::new(409, "show the patterns before uploading a photo"))?;
        for extension in PHOTO_EXTENSIONS {
            let stale = self.config.state_dir.join(format!("photo.{extension}"));
            if stale.exists() {
                std::fs::remove_file(&stale)
                    .with_context(|| format!("removing {}", stale.display()))?;
            }
        }
        let name = format!("photo.{}", photo_extension(body));
        self.save(&name, body)?;
        // Detection takes a moment on a large photo; keep the state unlocked.
        let path = self.config.state_dir.join(&name);
        let photo = detect::open_photo(&path)?;
        let detections = Detections {
            version: detect::DETECTIONS_VERSION,
            family: tag::FAMILY.to_owned(),
            detector: detect::BUILTIN_DETECTOR.to_owned(),
            photo: Photo {
                path: Some(name),
                width: photo.width(),
                height: photo.height(),
            },
            markers: detect::detect(&photo)?,
        };
        let calibration = calibration::solve(&manifest, &detections, SolveOptions::default());
        let photo_jpeg = Arc::new(preview::photo_jpeg(&photo)?);
        self.save("detections.json", detections.to_json()?.as_bytes())?;
        self.save("calibration.json", calibration.to_json()?.as_bytes())?;
        let mut state = self.state();
        state.calibration = Some(calibration);
        state.calibration_serial += 1;
        state.photo_jpeg = Some(photo_jpeg);
        Ok(self.status_locked(&state))
    }

    fn put_calibration(&self, body: &[u8]) -> Reply {
        let text = std::str::from_utf8(body).map_err(|_err| Failure::new(400, "not UTF-8"))?;
        let calibration = Calibration::from_json(text)?;
        self.save("calibration.json", calibration.to_json()?.as_bytes())?;
        let mut state = self.state();
        state.calibration = Some(calibration);
        state.calibration_serial += 1;
        Ok(self.status_locked(&state))
    }

    /// The reduced photo, decoded from the state directory after a restart.
    fn photo_jpeg(&self) -> Option<Arc<Vec<u8>>> {
        if let Some(jpeg) = self.state().photo_jpeg.clone() {
            return Some(jpeg);
        }
        // Decoding a large photo takes a moment, so keep the state unlocked.
        let jpeg = PHOTO_EXTENSIONS
            .iter()
            .map(|extension| self.config.state_dir.join(format!("photo.{extension}")))
            .find(|path| path.exists())
            .and_then(|path| detect::open_photo(&path).ok())
            .and_then(|photo| preview::photo_jpeg(&photo).ok())
            .map(Arc::new)?;
        self.state().photo_jpeg = Some(Arc::clone(&jpeg));
        Some(jpeg)
    }

    /// The corner editor for the current calibration.
    fn editor(&self) -> Reply {
        let calibration = self
            .state()
            .calibration
            .clone()
            .ok_or_else(|| Failure::new(404, "there is no calibration yet"))?;
        let photo_uri = self.photo_jpeg().map(|jpeg| preview::jpeg_data_uri(&jpeg));
        let page = preview::calibration_page(&calibration, photo_uri.as_deref())?;
        Ok(Response::html(page))
    }

    fn photo(&self) -> Reply {
        let jpeg = self
            .photo_jpeg()
            .ok_or_else(|| Failure::new(404, "there is no photo yet"))?;
        Ok(Response::new(200, "image/jpeg", jpeg.as_slice()))
    }

    /// Display `id`'s latest output, scaled down, while playing; no content
    /// before its first frame or while it does not play, as while playback
    /// restarts, which the simulation's frequent requests may catch.
    fn preview(&self, id: &str) -> Reply {
        let sink = self
            .state()
            .player
            .as_ref()
            .and_then(|player| player.preview_sink(id));
        let Some(sink) = sink else {
            return Ok(Response::no_content());
        };
        Ok(
            player::preview_jpeg(&sink)?.map_or_else(Response::no_content, |jpeg| {
                Response::new(200, "image/jpeg", jpeg)
            }),
        )
    }

    fn make_layout(self: &Arc<Self>, request: &LayoutRequest) -> Reply {
        let mut state = self.state();
        let calibration = state
            .calibration
            .as_ref()
            .ok_or_else(|| Failure::new(409, "upload a photo before making a layout"))?;
        let (mode, level) = match request.mode {
            LayoutMode::Viewpoint => (Mode::Viewpoint, request.anchor.clone()),
            LayoutMode::Planar => (
                Mode::Planar {
                    reference: request
                        .anchor
                        .clone()
                        .ok_or_else(|| Failure::new(400, "planar mode needs an anchor display"))?,
                    size: request.reference_size,
                },
                None,
            ),
        };
        let canvas = match request.canvas {
            CanvasRequest::Fit((width, height)) => CanvasSize::Fit { width, height },
            CanvasRequest::Tight { longest } => CanvasSize::Longest(longest),
        };
        let generated = canvas::layout(
            calibration,
            &LayoutOptions {
                mode,
                canvas,
                allow_incomplete: request.allow_incomplete,
                level,
                straighten: request.straighten,
            },
        )?;
        let mut generated = generated;
        // Masks are written by hand and belong to the displays: a new
        // layout keeps them.
        if let Some(previous) = &state.layout {
            for screen in &mut generated.layout.screens {
                if let Some(old) = previous.layout.screen(&screen.id) {
                    screen.mask.clone_from(&old.mask);
                }
            }
        }
        let json = generated
            .layout
            .to_json()
            .context("serializing the layout")?;
        self.save("layout.json", json.as_bytes())?;
        let serial = state.layout.as_ref().map_or(1, |layout| layout.serial + 1);
        state.layout = Some(GeneratedLayout {
            layout: generated.layout,
            omitted: generated.omitted,
            units_per_pixel: generated.units_per_pixel,
            serial,
            request: Some(request.clone()),
        });
        state.fill = request.fill;
        if state.show == Show::Play {
            // The running pipeline takes the layout: the browsers of
            // displays that stay keep their streams, and the others hear
            // from the next command.
            let State {
                player,
                layout,
                fill,
                ..
            } = &mut *state;
            let updated = match (player, layout) {
                (Some(player), Some(layout)) => player.update_layout(&layout.layout, *fill),
                _ => false,
            };
            if !updated {
                drop(state);
                return self.play();
            }
            self.bump(&mut state);
        }
        Ok(self.status_locked(&state))
    }

    fn layout_svg(&self) -> Reply {
        let state = self.state();
        let layout = state
            .layout
            .as_ref()
            .ok_or_else(|| Failure::new(404, "there is no layout yet"))?;
        Ok(Response::new(
            200,
            "image/svg+xml",
            preview::layout_svg(&layout.layout)?,
        ))
    }

    /// Stop a player without holding the state, waiting a little so its
    /// signalling ports are free again.
    fn retire(&self, player: Option<Player>) {
        if let Some(player) = player
            && !player.stop(STOP_TIMEOUT)
        {
            self.stuck_pipelines.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Start a player for the current layout; the old one must be retired.
    fn start_player(self: &Arc<Self>, state: &mut State) -> Result<(), Failure> {
        let layout = state
            .layout
            .as_ref()
            .ok_or_else(|| Failure::new(409, "make a layout before playing"))?;
        let run = self.runs.fetch_add(1, Ordering::Relaxed) + 1;
        *lock(&self.player_status) = PlayerStatus {
            run,
            state: "starting".to_owned(),
            error: None,
        };
        let status = Arc::clone(&self.player_status);
        let options = PlayerOptions {
            source: state.source.clone(),
            standby: state.standby.clone(),
            restart: state.restart,
            fill: state.fill,
            audio: state.sound.is_some(),
            ..self.config.player.clone()
        };
        let player = Player::start(
            &layout.layout,
            &options,
            &self.measurement,
            self.clock,
            move |event| {
                let mut status = lock(&status);
                if status.run != run {
                    return;
                }
                match event {
                    PlayerEvent::Playing => "playing".clone_into(&mut status.state),
                    PlayerEvent::Finished => "finished".clone_into(&mut status.state),
                    PlayerEvent::Failed(error) => {
                        "failed".clone_into(&mut status.state);
                        status.error = Some(error);
                    }
                }
            },
        )
        .map_err(|error| {
            let message = format!("{error:#}");
            *lock(&self.player_status) = PlayerStatus {
                run,
                state: "failed".to_owned(),
                error: Some(message.clone()),
            };
            Failure::new(500, message)
        })?;
        // Frame numbers start again with the new pipeline; the old run's
        // stamps would place its frames.
        lock(&self.measurement).clear();
        player.set_measuring(state.recording());
        // The new pipeline's displays reconnect before presenting frames.
        state.recording_since = state.recording().then(|| self.clock.now_ms());
        state.player = Some(player);
        state.show = Show::Play;
        self.bump(state);
        Ok(())
    }

    /// Play a pattern or a URL, switching to it while playing. Web pages are
    /// looked up first, which takes a few seconds, without holding the state.
    /// Pipeline descriptions are refused: over an unauthenticated page they
    /// could write files.
    fn set_source(&self, source: &Source) -> Reply {
        let invalid = match source {
            Source::Launch(_) => {
                Some("pipeline descriptions can only be given with --source".to_owned())
            }
            Source::Pattern(id) if source::pattern(id).is_none() => {
                Some(format!("no pattern {id:?}"))
            }
            Source::Uri(url) | Source::Media(url) | Source::Page(url) | Source::Whep(url)
                if !url.contains("://") =>
            {
                Some(format!(
                    "{url:?} is not a URL; use file:///path for a file on this server"
                ))
            }
            _ => None,
        };
        if let Some(message) = invalid {
            return Err(Failure::new(400, message));
        }
        let prepared = Prepared::prepare(source, &self.config.player.resolver)?;
        let mut state = self.state();
        if let Some(player) = &state.player {
            player.set_source(&prepared);
        }
        state.source = prepared;
        Ok(self.status_locked(&state))
    }

    /// Change whether the source restarts when it ends, and the standby
    /// pattern, while playing too.
    fn set_playback(&self, playback: &PlaybackRequest) -> Reply {
        if let Some(standby) = &playback.standby
            && source::pattern(standby).is_none()
        {
            return Err(Failure::new(400, format!("no pattern {standby:?}")));
        }
        let mut state = self.state();
        if let Some(restart) = playback.restart {
            state.restart = restart;
            if let Some(player) = &state.player {
                player.set_restart(restart);
            }
        }
        if let Some(standby) = &playback.standby
            && *standby != state.standby
        {
            if let Some(player) = &state.player {
                player.set_standby(standby)?;
            }
            standby.clone_into(&mut state.standby);
        }
        Ok(self.status_locked(&state))
    }

    /// Ask the browsers to keep this much video buffered, which trades
    /// smoothness on a jittery network for latency.
    fn set_buffer(&self, ms: Option<u32>) -> Reply {
        if ms.is_some_and(|ms| ms > MAX_BUFFER_MS) {
            return Err(Failure::new(
                400,
                format!("the buffer may be at most {MAX_BUFFER_MS} ms"),
            ));
        }
        let mut state = self.state();
        state.buffer_ms = ms;
        self.bump(&mut state);
        Ok(self.status_locked(&state))
    }

    /// Play the sound on `display`, or on none. Turning the sound on or off
    /// restarts playback, since every stream gains or loses it; moving it to
    /// another display only unmutes that one.
    fn set_sound(self: &Arc<Self>, display: Option<String>) -> Reply {
        if let Some(id) = &display {
            valid_id(id)?;
        }
        let mut state = self.state();
        let restart = state.player.is_some() && state.sound.is_some() != display.is_some();
        let previous = std::mem::replace(&mut state.sound, display);
        self.bump(&mut state);
        if restart {
            drop(state);
            // A player that cannot start this way, say without the Opus
            // encoder, would otherwise fail every later play too.
            return self.play().inspect_err(|_| {
                let mut state = self.state();
                state.sound = previous;
                self.bump(&mut state);
            });
        }
        Ok(self.status_locked(&state))
    }

    fn play(self: &Arc<Self>) -> Reply {
        let player = self.state().player.take();
        self.retire(player);
        let mut state = self.state();
        // Another request started a player while this one retired the old
        // one. It plays the current state too, since every change reaches
        // the running player, and replacing it here would stop it while
        // holding the state.
        if state.player.is_none() {
            self.start_player(&mut state)?;
        }
        Ok(self.status_locked(&state))
    }

    fn idle(&self) -> Response {
        let mut state = self.state();
        let player = state.player.take();
        state.measuring = false;
        state.aligner = None;
        state.recording_since = None;
        state.show = Show::Idle;
        *lock(&self.player_status) = PlayerStatus::default();
        self.bump(&mut state);
        drop(state);
        self.retire(player);
        self.status()
    }

    fn measure(&self, on: bool) -> Reply {
        let mut state = self.state();
        let player = state
            .player
            .as_ref()
            .ok_or_else(|| Failure::new(409, "start playing before measuring"))?;
        // Clear before stamping starts, or the first stamps are lost.
        if on && state.aligner.is_none() {
            lock(&self.measurement).clear();
        }
        player.set_measuring(on || state.aligner.is_some());
        state.measuring = on;
        state.note_recording(self.clock.now_ms());
        self.bump(&mut state);
        Ok(self.status_locked(&state))
    }

    /// Start or stop aligning the displays while playing.
    fn align(&self, on: bool) -> Reply {
        let mut state = self.state();
        let player = state
            .player
            .as_ref()
            .ok_or_else(|| Failure::new(409, "start playing before aligning"))?;
        if on && state.aligner.is_none() {
            if !state.measuring {
                lock(&self.measurement).clear();
            }
            player.set_measuring(true);
            state.aligner = Some(Aligner::new(self.clock.now_ms()));
        } else if !on {
            player.set_measuring(state.measuring);
            state.aligner = None;
        }
        state.note_recording(self.clock.now_ms());
        self.bump(&mut state);
        Ok(self.status_locked(&state))
    }

    /// Draw frame codes only while a display needs them, and take a step
    /// towards aligned displays when enough frames have been presented.
    fn tend_measurement(&self) {
        let mut state = self.state();
        let Some(player) = state.player.as_ref() else {
            return;
        };
        let displays: Vec<String> = player
            .routes()
            .iter()
            .map(|route| route.display.clone())
            .collect();
        let measurement = lock(&self.measurement);
        player.set_codes(measurement.needs_codes(&displays));
        let now = self.clock.now_ms();
        let stepped = state.aligner.as_mut().is_some_and(|aligner| {
            let summary = measurement.summary_since(&displays, aligner.since());
            aligner.step(&summary, now)
        });
        drop(measurement);
        if stepped {
            self.bump(&mut state);
        }
    }

    fn measurement(&self) -> Response {
        let state = self.state();
        let displays: Vec<String> = match &state.player {
            Some(player) => player
                .routes()
                .iter()
                .map(|route| route.display.clone())
                .collect(),
            None => state.displays.keys().cloned().collect(),
        };
        let summary = lock(&self.measurement).summary(&displays);
        Response::json(&json!({
            "measuring": state.measuring,
            "summary": summary,
            "aligning": state.aligning(),
        }))
    }

    fn status(&self) -> Response {
        let state = self.state();
        self.status_locked(&state)
    }

    /// Warnings for the displays whose pages play their streams, and which
    /// the pipeline sends frames, but which reported no presented frame for
    /// [`NO_FRAMES_MS`] while recording, once recording has run that long.
    fn silent_displays(&self, state: &State) -> HashMap<String, &'static str> {
        let now = self.clock.now_ms();
        let (Some(player), Some(since)) = (&state.player, state.recording_since) else {
            return HashMap::new();
        };
        if now - since < NO_FRAMES_MS {
            return HashMap::new();
        }
        let recent = |time: Option<f64>| time.is_some_and(|time| now - time < NO_FRAMES_MS);
        let measurement = lock(&self.measurement);
        player
            .routes()
            .iter()
            .filter(|route| {
                state.displays.get(&route.display).is_some_and(|display| {
                    display.online() && display.player.starts_with("playing")
                })
            })
            .filter(|route| {
                recent(measurement.last_stamped(&route.display))
                    && !recent(measurement.last_shown(&route.display))
            })
            .map(|route| {
                // A browser that sends reports presents frames, but none it
                // could tell apart; one that sends none draws nothing.
                let warning = if recent(measurement.last_report(&route.display)) {
                    "reports frames it cannot identify"
                } else {
                    "reported no frames for 5 s: is its window covered?"
                };
                (route.display.clone(), warning)
            })
            .collect()
    }

    /// Everything the control page shows.
    fn status_locked(&self, state: &State) -> Response {
        let layout = state.layout.as_ref();
        let silent = self.silent_displays(state);
        let displays: Vec<_> = state
            .displays
            .iter()
            .map(|(id, display)| {
                let mut warnings = Vec::new();
                if display.hidden && display.online() {
                    warnings.push("page hidden: its browser draws no frames".to_owned());
                } else if let Some(warning) = silent.get(id) {
                    warnings.push((*warning).to_owned());
                }
                if let Some(viewport) = &display.viewport {
                    if !viewport.fullscreen {
                        warnings.push("not fullscreen".to_owned());
                    }
                    let pattern = state.manifest.as_ref().and_then(|manifest| manifest.screen(id));
                    if state.show == Show::Patterns
                        && pattern.is_some_and(|screen| aspect_differs(viewport, screen.output))
                    {
                        warnings.push("viewport changed since the patterns were made".to_owned());
                    }
                    let screen = layout.and_then(|layout| layout.layout.screen(id));
                    if screen.is_some_and(|screen| aspect_differs(viewport, screen.output)) {
                        warnings.push("viewport differs from the layout's output".to_owned());
                    }
                }
                json!({
                    "id": id,
                    "online": display.online(),
                    "viewport": display.viewport,
                    "output": display.viewport.map(|viewport| viewport.output(self.config.max_output)),
                    "agent": display.agent,
                    "address": display.address,
                    "player": display.player,
                    "seen_seconds_ago": display.last_seen.elapsed().as_secs(),
                    "warnings": warnings,
                })
            })
            .collect();
        let calibration = state.calibration.as_ref().map(|calibration| {
            json!({
                "serial": state.calibration_serial,
                "photo": calibration.photo,
                "complete": calibration.screens.iter().all(|screen| screen.status != Status::Missing),
                "screens": calibration.screens.iter().map(|screen| json!({
                    "id": screen.id,
                    "output": screen.output,
                    "corners": screen.corners,
                    "status": screen.status,
                    "rms_error": screen.rms_error,
                    "max_error": screen.max_error,
                    "used": screen.markers.iter().filter(|marker| marker.state == calibration::MarkerState::Used).count(),
                    "markers": screen.markers.len(),
                    "notes": screen.notes,
                })).collect::<Vec<_>>(),
            })
        });
        let layout = layout.map(|layout| {
            json!({
                "serial": layout.serial,
                "canvas": layout.layout.canvas,
                "screens": layout.layout.screens.iter().map(|screen| json!({
                    "id": screen.id,
                    "output": screen.output,
                })).collect::<Vec<_>>(),
                "omitted": layout.omitted,
                "units_per_pixel": layout.units_per_pixel,
                "options": layout.request,
            })
        });
        let routes = state.player.as_ref().map(Player::routes);
        let source_status = state.player.as_ref().map(Player::source_status);
        Response::json(&json!({
            "show": state.show,
            "displays": displays,
            "manifest": state.manifest.as_ref().map(|manifest| json!({
                "serial": state.manifest_serial,
                "screens": manifest.screens.iter().map(|screen| json!({
                    "id": screen.id,
                    "output": screen.output,
                })).collect::<Vec<_>>(),
            })),
            "calibration": calibration,
            "layout": layout,
            "player": {
                "status": *lock(&self.player_status),
                "routes": routes,
                "source": source_status,
                "stuck_pipelines": self.stuck_pipelines.load(Ordering::Relaxed),
            },
            "measuring": state.measuring,
            "aligning": state.aligning(),
            "source": state.source,
            "standby": state.standby,
            "restart": state.restart,
            "patterns": source::PATTERNS,
            "buffer_ms": state.buffer_ms,
            "sound": state.sound,
        }))
    }
}

fn valid_id(id: &str) -> Result<&str, Failure> {
    if is_valid_screen_id(id) {
        Ok(id)
    } else {
        Err(Failure::new(
            400,
            format!("invalid display ID {id:?}; use letters, digits, '-', '_' and '.'"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_keep_the_viewport_aspect_ratio_and_stay_even() {
        let viewport = |width, height| Viewport {
            width,
            height,
            scale: 2.0,
            fullscreen: true,
        };
        assert_eq!(
            viewport(1512, 945).output(1920),
            Size {
                width: 1512,
                height: 946
            }
        );
        assert_eq!(
            viewport(3024, 1890).output(1920),
            Size {
                width: 1920,
                height: 1200
            }
        );
        assert_eq!(
            viewport(820, 1180).output(1920),
            Size {
                width: 820,
                height: 1180
            }
        );
        assert_eq!(
            viewport(1, 1).output(1920),
            Size {
                width: 2,
                height: 2
            }
        );
        assert!(!aspect_differs(
            &viewport(1512, 945),
            Size {
                width: 1512,
                height: 946
            }
        ));
        assert!(aspect_differs(
            &viewport(820, 1048),
            Size {
                width: 820,
                height: 1180
            }
        ));
    }

    #[test]
    fn photos_are_recognized_by_their_first_bytes() {
        assert_eq!(photo_extension(&[0xff, 0xd8, 0xff, 0xe0]), "jpg");
        assert_eq!(photo_extension(b"\x89PNG\r\n"), "png");
        assert_eq!(photo_extension(b"\0\0\0\x18ftypheic"), "heic");
        assert_eq!(photo_extension(b"GIF89a"), "img");
    }
}
