//! The playback pipeline: a source and a standby pattern, one shown at a
//! time, and one `wallwarp` branch per display.
//!
//! ```text
//! source  ! identity ! videoconvert [! videocrop] ! videoscale [! videorate] ! RGBA canvas [! clocksync] ! selector.
//! source  ! identity ! audioconvert ! audioresample ! S16 48 kHz stereo ! audiobuffersplit ! clocksync ! sound.   (with audio)
//! standby ! identity ! videoconvert ! videoscale ! RGBA canvas ! selector.
//! audiotestsrc wave=silence ! clocksync ! sound.   (with audio)
//! input-selector name=selector ! tee
//!   tee. ! queue ! wallwarp ! capsfilter ! tee   (per display)
//!     tee. ! queue ! webrtcsink meta="meta,display=<id>"
//!     tee. ! queue ! videorate ! videoscale ! capsfilter ! fakesink   (preview)
//! input-selector name=sound ! tee
//!   tee. ! queue ! webrtcsink's audio input   (per display, with audio)
//! ```
//!
//! The source and the standby are each a bin that is swapped while playing,
//! so the displays keep their streams when the source changes. The source
//! shows from its first frame; the standby shows while it connects, after it
//! ends, and when it fails or stalls. A source that fails is retried after a
//! growing delay, and one that ends restarts if asked to, so clips loop and
//! streams reconnect. Built-in patterns skip `videorate` and `clocksync`,
//! which only files and other sources need, and which add latency.
//!
//! Displays come and go with the layout: each has a branch of its own, added
//! to and removed from the running pipeline, so the others keep their
//! streams. A display's stream size is the caps of the `capsfilter` after its
//! `wallwarp`, which changes width and height as one step; `webrtcsink`
//! accepts that without a new WebRTC negotiation, its encoders start again
//! at the new size, and browsers follow.
//!
//! With audio, every display's stream carries the sound of what shows, or
//! silence for the standby and for sources without sound: `webrtcsink`
//! announces a display only once each of its inputs has had data. The
//! silence is paced like a clip rather than live, since a live source would
//! raise the latency every sink waits for, the video's too. Sound comes in
//! 20 ms buffers, one Opus frame each, so browsers receive it evenly and
//! buffer less of it, which their lip sync makes the video wait for.
//!
//! Every `webrtcsink` is its own producer, announced to the signalling server
//! with the display's ID in its metadata, and encodes separately for every
//! browser that connects. Each sink runs its own signalling server on its own
//! port by default: `webrtcsink`'s signalling client connects once, without
//! retrying, so several sinks cannot rely on a server another sink starts. An
//! external server, such as `gst-webrtc-signalling-server`, can serve all
//! displays instead.

use std::net::TcpListener;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use gst::prelude::*;
use gst_video::prelude::*;
use serde::{Deserialize, Serialize};
use wall_geometry::{Layout, Screen, Size};

use crate::drift::Measurement;
use crate::framecode::{self, CodeGeometry};
use crate::source::{self, Input, Prepared, Resolver, SLATE_TEXT, Source, Via};

/// Where browsers find the signalling server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signalling {
    /// Every display's sink serves on its own port, the first free ones from
    /// `first_port` in layout order, listening on `host`.
    BuiltIn { host: String, first_port: u16 },
    /// One server at this `ws://` or `wss://` URI for every display; browsers
    /// use the same URI, so it must name an address they can reach.
    External(String),
}

/// What ends every branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    WebRtc(Signalling),
    /// An `appsink` named `sink-<id>`, and with sound one named `sound-<id>`,
    /// for tests.
    AppSink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerOptions {
    pub source: Prepared,
    /// The pattern shown whenever the source is not.
    pub standby: String,
    /// Restart the source when it ends: clips loop and streams reconnect.
    pub restart: bool,
    pub fps: u32,
    pub sink: Sink,
    pub fill: Fill,
    /// Finds web pages' streams again when they fail, since their URLs expire.
    pub resolver: Resolver,
    /// Send the sound to every display. Browsers then hold each display's
    /// video back to keep it in sync with the sound, muted or not.
    pub audio: bool,
}

/// How a source whose aspect ratio differs from the canvas fills it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fill {
    /// Crop the source to the canvas's aspect ratio, so the displays show no
    /// empty bars and the source's edges may be cut off.
    #[default]
    Cover,
    /// Show the whole source, with black bars on two sides.
    Contain,
}

/// Longest side of a display's preview, in pixels.
const PREVIEW_PIXELS: u32 = 480;

/// Most previews per second.
const PREVIEW_RATE: i32 = 4;

/// How a display reaches its stream, as sent to its page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    /// The built-in server's port on the host serving the page.
    Port(u16),
    /// A signalling server URI.
    Uri(String),
    /// No browser endpoint, as with test sinks.
    None,
}

/// One display's branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Route {
    pub display: String,
    pub output: Size,
    pub endpoint: Endpoint,
    /// Which pipeline the stream belongs to; its producer announces the same
    /// number, so browsers skip producers of pipelines that failed to stop.
    pub generation: u32,
    /// Where measuring draws the frame code, if it fits.
    pub code: Option<CodeGeometry>,
}

/// Something the pipeline reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerEvent {
    Playing,
    Finished,
    Failed(String),
}

/// Milliseconds since a fixed start, shared with the browsers' clock sync.
#[derive(Clone, Copy, Debug)]
pub struct Clock(Instant);

impl Clock {
    #[must_use]
    pub fn new() -> Self {
        Self(Instant::now())
    }

    #[must_use]
    pub fn now_ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// The latest frame of a preview sink as JPEG, or `None` before the first.
pub fn preview_jpeg(sink: &gst::Element) -> Result<Option<Vec<u8>>> {
    let Some(sample) = sink.property::<Option<gst::Sample>>("last-sample") else {
        return Ok(None);
    };
    let caps = sample.caps().context("preview without caps")?;
    let info = gst_video::VideoInfo::from_caps(caps)?;
    let buffer = sample.buffer().context("preview without a buffer")?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_err| anyhow::anyhow!("cannot map a preview frame"))?;
    let stride = frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
        .context("preview without a stride")?;
    let (width, height) = (info.width(), info.height());
    let row = usize::try_from(width)? * 4;
    let rows = usize::try_from(height)?;
    let mut pixels = Vec::with_capacity(row * rows);
    for line in frame.plane_data(0)?.chunks(stride).take(rows) {
        pixels.extend(line.get(..row).context("short preview row")?);
    }
    let image = image::RgbaImage::from_raw(width, height, pixels).context("preview size")?;
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .to_rgb8()
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)?;
    Ok(Some(jpeg.into_inner()))
}

/// The frame number of a running time at `fps` frames per second.
#[must_use]
pub fn frame_number(running: gst::ClockTime, fps: u32) -> u64 {
    let frames = (u128::from(running.nseconds()) * u128::from(fps) + 500_000_000) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Asks the bus thread to finish.
const STOP_MESSAGE: &str = "wallplay-stop";

/// Fail early, with what to install, when an element WebRTC playback needs
/// is missing; otherwise every browser session fails on its own.
fn check_webrtc_elements(audio: bool) -> Result<()> {
    if gst::ElementFactory::find("webrtcsink").is_none() {
        bail!("webrtcsink is not available; install the rswebrtc plugin from gst-plugins-rs");
    }
    if ["nicesrc", "nicesink"]
        .iter()
        .any(|factory| gst::ElementFactory::find(factory).is_none())
    {
        bail!(
            "webrtcbin needs libnice's GStreamer elements, which are not available; install \
             libnice's GStreamer plugin, often packaged apart from GStreamer itself"
        );
    }
    if audio && gst::ElementFactory::find("opusenc").is_none() {
        bail!("sound needs opusenc, from GStreamer's opus plugin, which is not available");
    }
    Ok(())
}

/// Fail early when an element `options` needs is missing: those WebRTC
/// playback needs, and with sound the one every source's sound needs, since
/// otherwise the standby, a pattern, plays and every source fails and
/// retries for ever.
fn check_elements(options: &PlayerOptions) -> Result<()> {
    if matches!(options.sink, Sink::WebRtc(_)) {
        check_webrtc_elements(options.audio)?;
    }
    if options.audio && gst::ElementFactory::find("audiobuffersplit").is_none() {
        bail!("sound needs audiobuffersplit, from gst-plugins-bad, which is not available");
    }
    Ok(())
}

/// Numbers every pipeline started in this process.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// How long after a browser connects a session may still be negotiating;
/// see [`Player::stop`].
const SETTLE: Duration = Duration::from_millis(1500);

/// How many ports past the preferred one a signalling server may move to.
const PORT_SEARCH: u16 = 100;

/// The first port from `start` that `host` can listen on and is not in
/// `taken`. A pipeline whose teardown hangs keeps its signalling ports.
fn free_port(host: &str, start: u16, taken: &[u16]) -> Option<u16> {
    (start..=start.saturating_add(PORT_SEARCH))
        .filter(|port| !taken.contains(port))
        .find(|port| TcpListener::bind((host, *port)).is_ok())
}

/// A running pipeline. Dropping it stops playback; [`Player::stop`] does so
/// without blocking.
pub struct Player {
    pipeline: gst::Pipeline,
    inputs: Arc<Inputs>,
    /// In layout order.
    routes: Vec<Route>,
    branches: Vec<Branch>,
    /// Feeds every display's branch.
    tee: gst::Element,
    /// Feeds every display's sound, if there is sound.
    sound_tee: Option<gst::Element>,
    generation: u32,
    fps: u32,
    sink: Sink,
    measuring: Arc<AtomicBool>,
    drawing: Arc<AtomicBool>,
    measurement: Arc<Mutex<Measurement>>,
    clock: Clock,
    /// When a browser last started a session.
    last_consumer: Arc<Mutex<Option<Instant>>>,
    bus_thread: Option<JoinHandle<()>>,
}

/// A quad and the canvas size it is for.
type PendingQuad = Arc<Mutex<Option<(Size, String)>>>;

/// One display's elements in the pipeline.
struct Branch {
    display: String,
    warp: gst::Element,
    /// A quad for another canvas size, taken when frames of that size
    /// arrive, so no frame is warped with the other canvas's quad.
    pending: PendingQuad,
    /// Gives the warp's output the display's size.
    output: gst::Element,
    /// Gives the preview its size.
    preview: gst::Element,
    /// The `tee` pads feeding the branch.
    feeds: Vec<gst::Pad>,
    /// Upstream elements first.
    elements: Vec<gst::Element>,
}

/// Have `warp` take a quad queued in the returned slot once frames of the
/// quad's canvas size reach it.
fn follow_canvas(warp: &gst::Element) -> Result<PendingQuad> {
    let pending = PendingQuad::default();
    let waiting = Arc::clone(&pending);
    let element = warp.downgrade();
    warp.static_pad("sink")
        .context("wallwarp has no sink pad")?
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && let gst::EventView::Caps(caps) = event.view()
                && let Ok(video) = gst_video::VideoInfo::from_caps(caps.caps())
            {
                let mut waiting = lock(&waiting);
                let size = Size {
                    width: video.width(),
                    height: video.height(),
                };
                if let Some((canvas, quad)) = waiting.take_if(|(canvas, _)| *canvas == size)
                    && let Some(warp) = element.upgrade()
                {
                    gst::debug!(gst::CAT_DEFAULT, "new quad for {canvas:?}");
                    warp.set_property("quad", quad);
                }
            }
            gst::PadProbeReturn::Ok
        });
    Ok(pending)
}

/// Let frames of another size through to `webrtcsink`, whose inputs accept
/// them but answer caps queries with the current caps alone, which would
/// fail any renegotiation upstream. `pad` is what feeds the input.
fn allow_new_sizes(pad: &gst::Pad) {
    pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_pad, info| {
        let Some(gst::PadProbeData::Query(query)) = info.data.as_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        match query.view_mut() {
            gst::QueryViewMut::Caps(query) => {
                let caps = query.filter_owned().unwrap_or_else(gst::Caps::new_any);
                query.set_result(&caps);
                gst::PadProbeReturn::Handled
            }
            gst::QueryViewMut::AcceptCaps(query) => {
                query.set_result(true);
                gst::PadProbeReturn::Handled
            }
            _ => gst::PadProbeReturn::Ok,
        }
    });
}

/// A `capsfilter` whose caps change while playing. Frames of the previous
/// caps may already be on their way to it, and with the default mode it would
/// refuse them, stopping the stream for good.
fn size_filter(caps: gst::Caps) -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .property_from_str("caps-change-mode", "delayed")
        .build()?)
}

/// Caps giving a warp's output the size `output`.
fn output_caps(output: Size) -> Result<gst::Caps> {
    Ok(gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(i32::try_from(output.width)?)
        .height(i32::try_from(output.height)?)
        .build())
}

/// Caps of the preview of a display of size `output`.
fn preview_caps(output: Size) -> gst::Caps {
    let longest = output.width.max(output.height).max(1);
    let even = |value: u32| {
        let scaled = u64::from(value) * u64::from(PREVIEW_PIXELS.min(longest)) / u64::from(longest);
        i32::try_from((scaled / 2 * 2).max(2)).unwrap_or(2)
    };
    gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(even(output.width))
        .height(even(output.height))
        .build()
}

/// How long a `tee` may take to finish pushing to a branch being removed.
const DETACH_TIMEOUT: Duration = Duration::from_secs(1);

/// Unlink and release a `tee`'s pad between two buffers.
fn detach(pad: &gst::Pad) -> Result<()> {
    let (done, wait) = mpsc::channel();
    pad.add_probe(gst::PadProbeType::IDLE, move |pad, _info| {
        if let Some(peer) = pad.peer()
            && pad.unlink(&peer).is_err()
        {
            gst::warning!(gst::CAT_DEFAULT, "could not unlink {}", pad.name());
        }
        // The caller may have stopped waiting.
        if done.send(()).is_err() {
            gst::info!(gst::CAT_DEFAULT, "a branch detached after its timeout");
        }
        gst::PadProbeReturn::Remove
    });
    wait.recv_timeout(DETACH_TIMEOUT)
        .context("a branch did not detach in time")?;
    if let Some(tee) = pad.parent_element() {
        tee.release_request_pad(pad);
    }
    Ok(())
}

fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("creating {factory}; is its GStreamer plugin installed?"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long the source may go without a frame before the standby shows.
const STALL_STANDBY: Duration = Duration::from_secs(2);

/// How long the source may go without a frame before it restarts.
const STALL_RESTART: Duration = Duration::from_secs(10);

/// How long a source may take to show its first frame.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds before retrying a source that failed, the last repeating.
const RETRY_DELAYS: [u64; 4] = [1, 2, 5, 10];

/// A source that ends sooner than this after starting is retried after a
/// delay instead of restarted at once.
const SHORTEST_RUN: Duration = Duration::from_secs(1);

/// How far ahead of the clock a source that is not live starts, as slack
/// against network hiccups.
const START_MARGIN: gst::ClockTime = gst::ClockTime::from_mseconds(300);

/// How often the bus thread looks after the inputs.
const TICK: Duration = Duration::from_millis(250);

/// Posted by an input bin whose stream ended.
const ENDED_MESSAGE: &str = "wallplay-input-ended";

/// Name of the `clocksync` that plays sources in real time.
const PACE: &str = "pace";

/// Name of the `clocksync` that plays sources' sound in real time.
const AUDIO_PACE: &str = "audio-pace";

/// The format every sound is converted to, since `webrtcsink` refuses any
/// change of an audio format while playing, even a field such as the channel
/// mask that one input's caps leave out and another's carry.
pub const AUDIO_CAPS: &str =
    "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2,channel-mask=(bitmask)0x3";

/// Samples in 20 ms at 48 kHz, one Opus frame.
const AUDIO_SAMPLES: i32 = 960;

/// Prefix of input bins' names, which end in the attempt's number.
const INPUT_PREFIX: &str = "input-";

/// One of the selector's two inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Main,
    Standby,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Main => "source",
            Self::Standby => "standby",
        }
    }
}

/// What the source is doing, for the control page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SourceStatus {
    /// `starting`, `playing`, `retrying` or `ended`.
    pub state: String,
    /// Why it last failed, until it plays again.
    pub error: Option<String>,
    /// Whether the displays show the standby pattern.
    pub standby: bool,
    /// How often it restarted after ending or failing.
    pub restarts: u32,
}

/// Name of the `capsfilter` giving an input the canvas's size.
const CANVAS_CAPS: &str = "canvas";

/// Name of the `videocrop` cropping a source to the canvas.
const CROP: &str = "crop";

/// The canvas every input is converted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Canvas {
    size: Size,
    fps: u32,
    fill: Fill,
}

impl Canvas {
    /// Elements converting `input`'s video to the canvas, starting with an
    /// `identity` whose output [`align_start`] moves to the clock. Patterns
    /// are live and produce the canvas frame rate themselves; other sources
    /// may not, so `videorate` matches the rate, holding a frame to do so,
    /// and a `clocksync` named [`PACE`] plays them in real time.
    fn chain(&self, input: &Input) -> Result<Vec<gst::Element>> {
        let entry = make("identity")?;
        let convert = make("videoconvert")?;
        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", true)
            .build()?;
        let filter = gst::ElementFactory::make("capsfilter")
            .name(CANVAS_CAPS)
            .property("caps", self.caps()?)
            .build()?;
        if input.is_pattern() {
            return Ok(vec![entry, convert, scale, filter]);
        }
        // Cropping to the canvas's aspect ratio first leaves scaling no
        // borders to add.
        let crop = if self.fill == Fill::Cover {
            Some(cover_crop(&filter)?)
        } else {
            None
        };
        Ok([entry, convert]
            .into_iter()
            .chain(crop)
            .chain([
                scale,
                make("videorate")?,
                filter,
                gst::ElementFactory::make("clocksync").name(PACE).build()?,
            ])
            .collect())
    }

    fn caps(&self) -> Result<gst::Caps> {
        let fps = i32::try_from(self.fps).context("frame rate too large")?;
        Ok(gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::Rgba)
            .width(i32::try_from(self.size.width)?)
            .height(i32::try_from(self.size.height)?)
            .framerate(gst::Fraction::new(fps, 1))
            .pixel_aspect_ratio(gst::Fraction::new(1, 1))
            .build())
    }

    /// Convert the running chain in `bin` to this canvas, which has the same
    /// fill as the one it was made for.
    /// A cropping `videocrop` follows the canvas caps by itself.
    fn retarget(&self, bin: &gst::Bin) -> Result<()> {
        bin.by_name(CANVAS_CAPS)
            .context("an input without its canvas caps")?
            .set_property("caps", self.caps()?);
        Ok(())
    }
}

/// A `videocrop` cutting its input to the aspect ratio of the caps that
/// `canvas`, a capsfilter, asks for. The crop is worked out on the streaming
/// thread from each caps event and frame, so it always fits the frames it
/// cuts. (`aspectratiocrop` keeps the caps it saw when its ratio was set and
/// applies them to the next frame, so a source that changes size with the
/// canvas got cropped, and mapped, as if it still had its old size.)
fn cover_crop(canvas: &gst::Element) -> Result<gst::Element> {
    let crop = gst::ElementFactory::make("videocrop")
        .name(CROP)
        .build()
        .context("creating videocrop; is gst-plugins-good installed?")?;
    let pad = crop
        .static_pad("sink")
        .context("a videocrop without a sink pad")?;
    let canvas = canvas.downgrade();
    let element = crop.downgrade();
    // videocrop starts uncropped; setting a side, even to its current value,
    // makes it renegotiate, so only changes are set.
    let applied = Mutex::new(Sides::default());
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |pad, info| {
            let caps = match &info.data {
                Some(gst::PadProbeData::Event(event)) => match event.view() {
                    gst::EventView::Caps(caps) => Some(caps.caps_owned()),
                    _ => None,
                },
                Some(gst::PadProbeData::Buffer(_)) => pad.current_caps(),
                _ => None,
            };
            let (Some(caps), Some(canvas), Some(element)) =
                (caps, canvas.upgrade(), element.upgrade())
            else {
                return gst::PadProbeReturn::Ok;
            };
            let sides = gst_video::VideoInfo::from_caps(&caps)
                .ok()
                .zip(gst_video::VideoInfo::from_caps(&canvas.property::<gst::Caps>("caps")).ok())
                .map(|(frame, target)| Sides::covering(&frame, target.width(), target.height()))
                .unwrap_or_default();
            let mut applied = lock(&applied);
            if *applied != sides {
                element.set_property("top", sides.top);
                element.set_property("bottom", sides.bottom);
                element.set_property("left", sides.left);
                element.set_property("right", sides.right);
                *applied = sides;
            }
            gst::PadProbeReturn::Ok
        },
    )
    .context("watching the crop's input")?;
    Ok(crop)
}

/// Rows and columns a `videocrop` removes from each edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sides {
    top: i32,
    bottom: i32,
    left: i32,
    right: i32,
}

impl Sides {
    /// The equal crop on two opposite edges that leaves `frame` with the
    /// aspect ratio of a `width` × `height` canvas of square pixels, or none
    /// when it already has it or would lose everything. Rounds as
    /// `aspectratiocrop` does, toward a slightly larger remainder.
    fn covering(frame: &gst_video::VideoInfo, width: u32, height: u32) -> Self {
        let (w, h) = (i64::from(frame.width()), i64::from(frame.height()));
        let par = frame.par();
        let (pn, pd) = (i64::from(par.numer()), i64::from(par.denom()));
        let (tw, th) = (i64::from(width), i64::from(height));
        if w == 0 || h == 0 || tw == 0 || th == 0 || pn <= 0 || pd <= 0 {
            return Self::default();
        }
        // Compare w·pn / (h·pd) with tw / th without rounding.
        let shown = w * pn * th;
        let wanted = tw * h * pd;
        let side = |excess: i64, scale: i64, full: i64| {
            let cut = excess / (2 * scale);
            (cut < full / 2).then(|| i32::try_from(cut).ok()).flatten()
        };
        match wanted.cmp(&shown) {
            // Too tall: keep w·pn·th / (tw·pd) of the h rows.
            std::cmp::Ordering::Greater => side(wanted - shown, tw * pd, h)
                .map(|cut| Self {
                    top: cut,
                    bottom: cut,
                    ..Self::default()
                })
                .unwrap_or_default(),
            // Too wide: keep h·pd·tw / (th·pn) of the w columns.
            std::cmp::Ordering::Less => side(shown - wanted, th * pn, w)
                .map(|cut| Self {
                    left: cut,
                    right: cut,
                    ..Self::default()
                })
                .unwrap_or_default(),
            std::cmp::Ordering::Equal => Self::default(),
        }
    }
}

/// Elements converting an input's sound to [`AUDIO_CAPS`] in buffers of
/// `audiobuffersplit`'s default 20 ms, starting with an `identity` that
/// [`align_start`] gives the video's offset, and ending in a `clocksync`
/// named [`AUDIO_PACE`] that plays it in real time, as [`PACE`] plays the
/// video.
fn audio_chain() -> Result<Vec<gst::Element>> {
    Ok(vec![
        make("identity")?,
        make("audioconvert")?,
        make("audioresample")?,
        gst::ElementFactory::make("capsfilter")
            .property("caps", gst::Caps::from_str(AUDIO_CAPS)?)
            .build()?,
        make("audiobuffersplit")?,
        gst::ElementFactory::make("clocksync")
            .name(AUDIO_PACE)
            .build()?,
    ])
}

/// How far to move a stream whose first buffer is `buffer`, leaving `pad`,
/// so that it starts with the next canvas frame of `period`, or a little
/// later if it is not live: a file starts at running time zero wherever the
/// pipeline's clock is. Starting on a frame of the canvas keeps frame
/// numbers from repeating where the selector switches. Also returns whether
/// the stream is live.
fn start_offset(
    pad: &gst::Pad,
    upstream: &gst::Pad,
    buffer: &gst::BufferRef,
    period: gst::ClockTime,
) -> Option<(i64, bool)> {
    let element = pad.parent_element()?;
    let now = element.clock()?.time().checked_sub(element.base_time()?)?;
    let segment = pad.sticky_event::<gst::event::Segment>(0)?;
    let running = segment
        .segment()
        .downcast_ref::<gst::format::Time>()?
        .to_running_time(buffer.pts()?)?;
    let mut latency = gst::query::Latency::new();
    let live = upstream.peer_query(&mut latency) && latency.result().0;
    let start = if live { now } else { now + START_MARGIN };
    let period = period.nseconds().max(1);
    let start = (start.nseconds() / period + 1).checked_mul(period)?;
    let offset = i64::try_from(start).ok()? - i64::try_from(running.nseconds()).ok()?;
    Some((offset.max(0), live))
}

/// The running time, in nanoseconds, that the segment `pad` carries gives
/// timestamp `pts`, which may lie before the segment.
fn running_nanos(pad: &gst::Pad, pts: gst::ClockTime) -> Option<i64> {
    let segment = pad.sticky_event::<gst::event::Segment>(0)?;
    let running = segment
        .segment()
        .downcast_ref::<gst::format::Time>()?
        .to_running_time_full(pts)?;
    let nanos = i64::try_from(running.abs().nseconds()).ok()?;
    Some(if running.is_positive() { nanos } else { -nanos })
}

/// Where an input's video starts, which its first buffer decides and its
/// sound follows.
#[derive(Clone, Copy)]
struct Anchor {
    /// How far the video moved.
    offset: i64,
    /// The video's first timestamp and the running time it plays at.
    first: Option<(gst::ClockTime, i64)>,
}

impl Anchor {
    /// How far to move the sound leaving `pad` so that it plays the video's
    /// first timestamp when the video does: streams fetched apart, such as a
    /// live page's video and audio playlists, start at different positions
    /// but keep one timeline in their timestamps. Without timestamps to go
    /// by, the sound moves as far as the video.
    fn sound_offset(self, pad: &gst::Pad) -> i64 {
        self.first
            .and_then(|(pts, plays_at)| Some(plays_at - running_nanos(pad, pts)?))
            .unwrap_or(self.offset)
    }
}

/// An input's start, which its first video buffer decides and its sound
/// waits for.
#[derive(Default)]
struct Start {
    anchor: Option<Anchor>,
    /// Sound waiting for the anchor: its pad and the probe blocking it.
    waiting: Option<(gst::Pad, gst::PadProbeId)>,
}

/// Move the stream leaving `entry` to the clock on its first buffer. An
/// input added to a playing pipeline would otherwise start in the past, and
/// its `paces` would let it race to catch up. A live stream comes in real
/// time already, and waiting for its latency on top would only delay it, so
/// it passes its `paces` without waiting. The sound leaving `audio`, if the
/// input has some, waits for the video's start and moves to stay with the
/// picture; see [`Anchor::sound_offset`].
fn align_start(
    entry: &gst::Element,
    audio: Option<&gst::Element>,
    paces: Vec<gst::Element>,
    fps: u32,
) -> Result<()> {
    let period = gst::ClockTime::SECOND
        .mul_div_floor(1, u64::from(fps.max(1)))
        .context("frame rate too large")?;
    let start: Arc<Mutex<Start>> = Arc::default();
    if let Some(audio) = audio {
        let start = Arc::clone(&start);
        audio
            .static_pad("src")
            .context("identity has no source pad")?
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BLOCK,
                move |pad, info| {
                    let mut start = lock(&start);
                    if let Some(anchor) = start.anchor {
                        pad.set_offset(anchor.sound_offset(pad));
                        return gst::PadProbeReturn::Remove;
                    }
                    // Blocked until the video's probe sets the anchor and
                    // removes this one.
                    start.waiting = info.id.take().map(|id| (pad.clone(), id));
                    gst::PadProbeReturn::Ok
                },
            );
    }
    let src = entry
        .static_pad("src")
        .context("identity has no source pad")?;
    let sink = entry
        .static_pad("sink")
        .context("identity has no sink pad")?;
    // A blocking probe runs before the pad sends its sticky events again,
    // so the first buffer already goes out with the new offset.
    src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BLOCK,
        move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let (offset, live) = start_offset(pad, &sink, buffer, period).unwrap_or((0, false));
                let anchor = Anchor {
                    offset,
                    first: buffer
                        .pts()
                        .and_then(|pts| Some((pts, running_nanos(pad, pts)? + offset))),
                };
                pad.set_offset(offset);
                if live {
                    for pace in &paces {
                        pace.set_property("sync", false);
                    }
                }
                let waiting = {
                    let mut start = lock(&start);
                    start.anchor = Some(anchor);
                    start.waiting.take()
                };
                if let Some((sound, probe)) = waiting {
                    sound.set_offset(anchor.sound_offset(&sound));
                    sound.remove_probe(probe);
                }
            }
            gst::PadProbeReturn::Remove
        },
    );
    Ok(())
}

/// Link `source`'s pads to an input's chains as they appear: its first
/// video stream to `video`, its first sound to `audio` if the displays get
/// sound, and anything else, such as a second language, to a `fakesink`.
fn link_dynamic(
    source: &gst::Element,
    video: &gst::Element,
    audio: Option<&gst::Element>,
    bin: &gst::Bin,
) -> Result<()> {
    let video = video
        .static_pad("sink")
        .context("identity has no sink pad")?;
    let audio = audio
        .map(|audio| audio.static_pad("sink").context("identity has no sink pad"))
        .transpose()?;
    let bin = bin.downgrade();
    source.connect_pad_added(move |_source, pad| {
        let entry = if source::is_audio(pad) {
            audio.as_ref()
        } else {
            Some(&video)
        };
        if let Some(entry) = entry
            && !entry.is_linked()
            && pad.link(entry).is_ok()
        {
            return;
        }
        let Some(bin) = bin.upgrade() else {
            return;
        };
        let linked = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|fakesink| {
                bin.add(&fakesink)?;
                fakesink.sync_state_with_parent()?;
                let fakesink_pad = fakesink.static_pad("sink").context("fakesink has no pad")?;
                pad.link(&fakesink_pad)?;
                Ok(())
            });
        if let Err(error) = linked {
            gst::warning!(gst::CAT_DEFAULT, "could not discard a stream: {error:#}");
        }
    });
    Ok(())
}

/// Which input the selector shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Showing {
    Standby,
    /// The current source.
    Main,
    /// The source being replaced, until the new one's first frame.
    Outgoing,
}

/// The sound's selector, which follows the video's.
struct Sound {
    selector: gst::Element,
    /// Where the silence joins, played for inputs without sound.
    silence: gst::Pad,
    /// Where each input's sound joins, by where its video joins, once its
    /// sound flows.
    inputs: Vec<(gst::Pad, gst::Pad)>,
}

/// The selector and which input it shows.
struct Selection {
    selector: gst::Element,
    /// Where each input joins the selector.
    main: Option<gst::Pad>,
    standby: Option<gst::Pad>,
    outgoing: Option<gst::Pad>,
    showing: Showing,
    sound: Option<Sound>,
}

impl Selection {
    fn show(&mut self, showing: Showing) {
        self.showing = showing;
        if let Some(pad) = self.shown() {
            self.selector.set_property("active-pad", pad);
        }
        self.follow();
    }

    /// Where the input showing joins.
    fn shown(&self) -> Option<&gst::Pad> {
        match self.showing {
            Showing::Main => self.main.as_ref(),
            Showing::Standby => self.standby.as_ref(),
            Showing::Outgoing => self.outgoing.as_ref(),
        }
    }

    /// Play the sound of the input showing, or silence.
    fn follow(&self) {
        let Some(sound) = &self.sound else {
            return;
        };
        let shown = self.shown();
        let pad = sound
            .inputs
            .iter()
            .find(|(video, _)| Some(video) == shown)
            .map_or(&sound.silence, |(_, audio)| audio);
        if sound
            .selector
            .property::<Option<gst::Pad>>("active-pad")
            .as_ref()
            != Some(pad)
        {
            sound.selector.set_property("active-pad", pad);
        }
    }

    /// The input whose video joins at `video` has sound, joining at `audio`.
    fn sound_flows(&mut self, video: gst::Pad, audio: gst::Pad) {
        if let Some(sound) = &mut self.sound {
            sound.inputs.push((video, audio));
        }
        self.follow();
    }

    /// The input whose video joined at `video` stops.
    fn sound_stops(&mut self, video: &gst::Pad) {
        if let Some(sound) = &mut self.sound {
            sound.inputs.retain(|(joined, _)| joined != video);
        }
        self.follow();
    }

    /// A frame left the input that joins at `pad`.
    fn frame(&mut self, pad: &gst::Pad) {
        if self.showing != Showing::Main && self.main.as_ref() == Some(pad) {
            self.show(Showing::Main);
        }
    }

    /// The source that joins at `pad` stalled.
    fn stalled(&mut self, pad: &gst::Pad) {
        let shown = match self.showing {
            Showing::Main => self.main.as_ref(),
            Showing::Outgoing => self.outgoing.as_ref(),
            Showing::Standby => None,
        };
        if shown == Some(pad) {
            self.show(Showing::Standby);
        }
    }

    /// The source joining at `pad` is being replaced: keep showing it, if
    /// it shows, until the next source's first frame.
    fn hand_over(&mut self, pad: &gst::Pad) {
        if self.main.as_ref() != Some(pad) {
            return;
        }
        self.outgoing = self.main.take();
        if self.showing == Showing::Main {
            self.showing = Showing::Outgoing;
        }
    }

    /// The source being replaced, joining at `pad`, stops.
    fn drop_outgoing(&mut self, pad: &gst::Pad) {
        if self.outgoing.as_ref() != Some(pad) {
            return;
        }
        if self.showing == Showing::Outgoing {
            self.show(Showing::Standby);
        }
        self.outgoing = None;
    }

    /// `role` now joins at `pad`, or not at all; a source shows from its
    /// first frame.
    fn join(&mut self, role: Role, pad: Option<gst::Pad>) {
        match role {
            Role::Main => {
                self.main = pad;
                if self.showing == Showing::Main {
                    self.show(Showing::Standby);
                }
            }
            Role::Standby => {
                self.standby = pad;
                if self.showing == Showing::Standby {
                    self.show(Showing::Standby);
                }
            }
        }
    }
}

/// What went wrong in an input, briefly: the error and the last line of its
/// details, which often says more, as in "Not Found" and "Not Found (404),
/// URL: …".
fn input_error(error: &gst::message::Error) -> String {
    let message = error.error().to_string();
    let detail = error
        .debug()
        .and_then(|debug| debug.lines().last().map(str::trim).map(str::to_owned))
        .filter(|detail| !detail.is_empty() && *detail != message && detail.len() <= 200);
    match detail {
        Some(detail) => format!("{message} ({detail})"),
        None => message,
    }
}

/// Watch frames and the end of the stream leaving an input's bin: frames
/// keep the time of the latest one and show the source; the end of the
/// stream is held back from the selector and reported on the bus.
fn watch_output(
    ghost: &gst::Pad,
    joined: gst::Pad,
    bin: &gst::Bin,
    last_frame: Arc<Mutex<Option<Instant>>>,
    selection: Arc<Mutex<Selection>>,
) {
    let bin = bin.downgrade();
    ghost.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_pad, info| match &info.data {
            Some(gst::PadProbeData::Buffer(_)) => {
                *lock(&last_frame) = Some(Instant::now());
                lock(&selection).frame(&joined);
                gst::PadProbeReturn::Ok
            }
            Some(gst::PadProbeData::Event(event)) if event.type_() == gst::EventType::Eos => {
                if let Some(bin) = bin.upgrade() {
                    let ended = gst::message::Application::builder(gst::Structure::new_empty(
                        ENDED_MESSAGE,
                    ))
                    .src(&bin)
                    .build();
                    if bin.post_message(ended).is_err() {
                        gst::warning!(gst::CAT_DEFAULT, "could not report an ended input");
                    }
                }
                gst::PadProbeReturn::Drop
            }
            _ => gst::PadProbeReturn::Ok,
        },
    );
}

/// Watch the sound leaving an input's bin: its first buffer lets it play
/// whenever the input shows, and the end of the stream is held back from the
/// selector, as the video's is.
fn watch_sound(
    ghost: &gst::Pad,
    video: gst::Pad,
    audio: gst::Pad,
    selection: Arc<Mutex<Selection>>,
) {
    let flowing = AtomicBool::new(false);
    ghost.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_pad, info| match &info.data {
            Some(gst::PadProbeData::Buffer(_)) => {
                if !flowing.swap(true, Ordering::Relaxed) {
                    lock(&selection).sound_flows(video.clone(), audio.clone());
                }
                gst::PadProbeReturn::Ok
            }
            Some(gst::PadProbeData::Event(event)) if event.type_() == gst::EventType::Eos => {
                gst::PadProbeReturn::Drop
            }
            _ => gst::PadProbeReturn::Ok,
        },
    );
}

/// Join the sound leaving `ghost` to the sound's `selector`; returns where.
fn join_sound(ghost: &gst::GhostPad, selector: &gst::Element) -> Result<gst::Pad> {
    let pad = selector
        .request_pad_simple("sink_%u")
        .context("input-selector has no input to give")?;
    if let Err(error) = ghost.link(&pad) {
        selector.release_request_pad(&pad);
        return Err(error.into());
    }
    Ok(pad)
}

/// Fill an input's `bin` with `input`'s sources and the chains converting
/// them for `canvas`, its sound too if `with_sound` is set; returns the
/// bin's ghost pads for the video, `src`, and the sound, `audio`.
fn assemble(
    bin: &gst::Bin,
    input: &Input,
    canvas: Canvas,
    with_sound: bool,
) -> Result<(gst::GhostPad, Option<gst::GhostPad>)> {
    let sources = input.elements(canvas.size, with_sound)?;
    let chain = canvas.chain(input)?;
    let audio = with_sound.then(audio_chain).transpose()?;
    bin.add_many(&sources)?;
    for chain in std::iter::once(&chain).chain(&audio) {
        bin.add_many(chain)?;
        gst::Element::link_many(chain)?;
    }
    let (Some(entry), Some(last)) = (chain.first(), chain.last()) else {
        bail!("empty canvas chain");
    };
    let audio_entry = audio.as_ref().and_then(|audio| audio.first());
    for source in &sources {
        if source.static_pad("src").is_none() {
            link_dynamic(source, entry, audio_entry, bin)?;
            continue;
        }
        source
            .link_pads(Some("src"), entry, Some("sink"))
            .context("the source pipeline has no video output")?;
        if let Some(audio_entry) = audio_entry
            && source.static_pad("audio").is_some()
        {
            source.link_pads(Some("audio"), audio_entry, Some("sink"))?;
        }
    }
    let ghost = gst::GhostPad::with_target(
        &last
            .static_pad("src")
            .context("capsfilter has no source pad")?,
    )?;
    bin.add_pad(&ghost)?;
    let audio_ghost = match audio.as_ref().and_then(|audio| audio.last()) {
        Some(last) => {
            let target = last
                .static_pad("src")
                .context("clocksync has no source pad")?;
            let ghost = gst::GhostPad::builder_with_target(&target)?
                .name("audio")
                .build();
            bin.add_pad(&ghost)?;
            Some(ghost)
        }
        None => None,
    };
    let paces = [PACE, AUDIO_PACE]
        .into_iter()
        .filter_map(|name| bin.by_name(name))
        .collect();
    align_start(entry, audio_entry, paces, canvas.fps)?;
    Ok((ghost, audio_ghost))
}

/// A URL's file name, or its host if it has none, for the slate.
fn short_name(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let host = rest.split('/').next().unwrap_or(rest);
    let file = rest
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|file| *file != host && !file.is_empty());
    file.unwrap_or(host).to_owned()
}

/// One attempt at playing an input.
struct Attempt {
    bin: gst::Bin,
    /// Where it joins the selector.
    pad: gst::Pad,
    /// Where its sound joins the sound's selector, if the displays get sound
    /// and it is not a pattern.
    sound: Option<gst::Pad>,
    started: Instant,
    /// When its latest frame left the bin.
    last_frame: Arc<Mutex<Option<Instant>>>,
    /// What its slate, if it is one, last said.
    slate: String,
}

impl Attempt {
    fn owns(&self, object: &gst::Object) -> bool {
        object == self.bin.upcast_ref::<gst::Object>() || object.has_as_ancestor(&self.bin)
    }
}

/// One input of the selector and what plays on it.
struct Feed {
    role: Role,
    source: Prepared,
    attempt: Option<Attempt>,
    /// The attempt being replaced, shown until `attempt`'s first frame.
    outgoing: Option<Attempt>,
    /// Numbers attempts, naming their bins.
    serial: u64,
    /// Failures since it last played.
    failures: u32,
    retry_at: Option<Instant>,
    /// Finding a web page's stream again.
    resolving: Option<JoinHandle<Result<Prepared>>>,
}

impl Feed {
    fn new(role: Role, source: Prepared) -> Self {
        Self {
            role,
            source,
            attempt: None,
            outgoing: None,
            serial: 0,
            failures: 0,
            retry_at: None,
            resolving: None,
        }
    }

    /// Whether `object` belongs to its current attempt.
    fn owns(&self, object: &gst::Object) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| attempt.owns(object))
    }

    /// Whether `object` belongs to the attempt being replaced.
    fn owned_by_outgoing(&self, object: &gst::Object) -> bool {
        self.outgoing
            .as_ref()
            .is_some_and(|attempt| attempt.owns(object))
    }
}

struct Feeds {
    main: Feed,
    standby: Feed,
    /// Set once the player stops, so nothing restarts.
    stopped: bool,
}

/// Everything that swaps and restarts the inputs, shared by the player, its
/// bus thread and pad probes.
///
/// Lock order: `feeds`, then `selection`, then `status`. Pad probes take only
/// `selection` and an attempt's `last_frame`.
struct Inputs {
    pipeline: gst::Pipeline,
    selector: gst::Element,
    /// The sound's selector, if the displays get sound.
    sound: Option<gst::Element>,
    canvas: Mutex<Canvas>,
    resolver: Resolver,
    restart: AtomicBool,
    /// Tells the bus thread to finish.
    halted: AtomicBool,
    feeds: Mutex<Feeds>,
    selection: Arc<Mutex<Selection>>,
    status: Mutex<SourceStatus>,
}

impl Inputs {
    /// Inputs joining `selector`, and `sound`'s selector, with the pad its
    /// silence joins at, if the displays get sound.
    fn new(
        pipeline: &gst::Pipeline,
        selector: gst::Element,
        sound: Option<(gst::Element, gst::Pad)>,
        canvas: Size,
        options: &PlayerOptions,
    ) -> Result<Self> {
        Ok(Self {
            pipeline: pipeline.clone(),
            selector: selector.clone(),
            sound: sound.as_ref().map(|(selector, _)| selector.clone()),
            canvas: Mutex::new(Canvas {
                size: canvas,
                fps: options.fps,
                fill: options.fill,
            }),
            resolver: options.resolver.clone(),
            restart: AtomicBool::new(options.restart),
            halted: AtomicBool::new(false),
            feeds: Mutex::new(Feeds {
                main: Feed::new(Role::Main, options.source.clone()),
                standby: Feed::new(Role::Standby, Prepared::pattern(&options.standby)?),
                stopped: false,
            }),
            selection: Arc::new(Mutex::new(Selection {
                selector,
                main: None,
                standby: None,
                outgoing: None,
                showing: Showing::Standby,
                sound: sound.map(|(selector, silence)| Sound {
                    selector,
                    silence,
                    inputs: Vec::new(),
                }),
            })),
            status: Mutex::new(SourceStatus::default()),
        })
    }

    fn feed(feeds: &mut Feeds, role: Role) -> &mut Feed {
        match role {
            Role::Main => &mut feeds.main,
            Role::Standby => &mut feeds.standby,
        }
    }

    /// Start the standby, pattern `id`, which must play.
    fn start_standby(&self, id: &str) -> Result<()> {
        let mut feeds = lock(&self.feeds);
        self.start(&mut feeds.standby);
        ensure!(
            feeds.standby.attempt.is_some(),
            "the standby pattern {id:?} does not play"
        );
        Ok(())
    }

    /// Start a new attempt at `feed`'s source.
    fn start(&self, feed: &mut Feed) {
        feed.serial += 1;
        feed.retry_at = None;
        match self.attach(feed.role, feed.serial, &feed.source.input) {
            Ok(attempt) => {
                feed.attempt = Some(attempt);
                if feed.role == Role::Main {
                    "starting".clone_into(&mut lock(&self.status).state);
                }
            }
            Err(error) => self.fail(feed, &format!("{error:#}")),
        }
    }

    /// Build a bin playing `input`, join it to the selectors and start it.
    fn attach(&self, role: Role, serial: u64, input: &Input) -> Result<Attempt> {
        let bin = gst::Bin::with_name(&format!("{INPUT_PREFIX}{}-{serial}", role.name()));
        // A source still prerolling, or one that never will, must not take
        // the pipeline back to paused, which would stop the standby too.
        bin.set_property("async-handling", true);
        let configured = input.clone();
        bin.connect_deep_element_added(move |_bin, _parent, element| {
            configured.configure(element);
        });
        let canvas = *lock(&self.canvas);
        let with_sound = self.sound.is_some() && !input.is_pattern();
        let (ghost, audio_ghost) = assemble(&bin, input, canvas, with_sound)?;
        self.pipeline.add(&bin)?;
        let sound = match (&audio_ghost, &self.sound) {
            (Some(ghost), Some(selector)) => match join_sound(ghost, selector) {
                Ok(sound) => Some(sound),
                Err(error) => {
                    self.pipeline.remove(&bin)?;
                    return Err(error);
                }
            },
            _ => None,
        };
        let Some(pad) = self.selector.request_pad_simple("sink_%u") else {
            self.release_sound(sound.as_ref());
            self.pipeline.remove(&bin)?;
            bail!("input-selector has no input to give");
        };
        let last_frame = Arc::new(Mutex::new(None));
        watch_output(
            ghost.upcast_ref(),
            pad.clone(),
            &bin,
            Arc::clone(&last_frame),
            Arc::clone(&self.selection),
        );
        if let (Some(ghost), Some(sound)) = (&audio_ghost, &sound) {
            watch_sound(
                ghost.upcast_ref(),
                pad.clone(),
                sound.clone(),
                Arc::clone(&self.selection),
            );
        }
        if let Err(error) = ghost.link(&pad) {
            self.selector.release_request_pad(&pad);
            self.release_sound(sound.as_ref());
            self.pipeline.remove(&bin)?;
            return Err(error.into());
        }
        lock(&self.selection).join(role, Some(pad.clone()));
        // A source that cannot start, such as a missing file, posts why on
        // the bus, which retries it.
        if bin.sync_state_with_parent().is_err() {
            gst::info!(gst::CAT_DEFAULT, "an input did not start");
        }
        Ok(Attempt {
            bin,
            pad,
            sound,
            started: Instant::now(),
            last_frame,
            slate: String::new(),
        })
    }

    /// Give back where an input's sound joined the sound's selector.
    fn release_sound(&self, sound: Option<&gst::Pad>) {
        if let (Some(pad), Some(selector)) = (sound, &self.sound) {
            selector.release_request_pad(pad);
        }
    }

    /// Stop `feed`'s current attempt on another thread, since stopping a
    /// network source can take a while.
    fn retire(&self, feed: &mut Feed) {
        let Some(attempt) = feed.attempt.take() else {
            return;
        };
        {
            let mut selection = lock(&self.selection);
            let joined = match feed.role {
                Role::Main => selection.main.as_ref(),
                Role::Standby => selection.standby.as_ref(),
            };
            if joined == Some(&attempt.pad) {
                selection.join(feed.role, None);
            }
        }
        self.stop_attempt(attempt);
    }

    /// Stop the attempt `feed` is replacing, if any.
    fn retire_outgoing(&self, feed: &mut Feed) {
        if let Some(attempt) = feed.outgoing.take() {
            lock(&self.selection).drop_outgoing(&attempt.pad);
            self.stop_attempt(attempt);
        }
    }

    /// Replace `feed`'s attempt with a new one, keeping a source on screen,
    /// if it shows, until the new one's first frame.
    fn hand_over(&self, feed: &mut Feed) {
        let showing = lock(&self.selection).showing;
        match (feed.role, showing) {
            // An earlier source still shows while this one starts.
            (Role::Main, Showing::Outgoing) => self.retire(feed),
            (Role::Main, Showing::Main) => {
                self.retire_outgoing(feed);
                if let Some(attempt) = feed.attempt.take() {
                    lock(&self.selection).hand_over(&attempt.pad);
                    feed.outgoing = Some(attempt);
                }
            }
            _ => {
                self.retire_outgoing(feed);
                self.retire(feed);
            }
        }
        self.start(feed);
    }

    fn stop_attempt(&self, attempt: Attempt) {
        lock(&self.selection).sound_stops(&attempt.pad);
        attempt.bin.set_locked_state(true);
        let (pipeline, selector, sound, selection) = (
            self.pipeline.clone(),
            self.selector.clone(),
            self.sound.clone(),
            Arc::clone(&self.selection),
        );
        let stopping = std::thread::Builder::new()
            .name("wallplay-retire".to_owned())
            .spawn(move || {
                if attempt.bin.set_state(gst::State::Null).is_err() {
                    gst::warning!(gst::CAT_DEFAULT, "an input did not stop cleanly");
                }
                // The sound's first buffer may have left the bin between
                // the selection forgetting it and the bin stopping, joining
                // it again; nothing flows out of a stopped bin.
                lock(&selection).sound_stops(&attempt.pad);
                if let Some(ghost) = attempt.bin.static_pad("src")
                    && ghost.unlink(&attempt.pad).is_err()
                {
                    gst::warning!(gst::CAT_DEFAULT, "an input was not linked");
                }
                selector.release_request_pad(&attempt.pad);
                if let (Some(pad), Some(sound)) = (&attempt.sound, &sound) {
                    if let Some(ghost) = attempt.bin.static_pad("audio")
                        && ghost.unlink(pad).is_err()
                    {
                        gst::warning!(gst::CAT_DEFAULT, "an input's sound was not linked");
                    }
                    sound.release_request_pad(pad);
                }
                if pipeline.remove(&attempt.bin).is_err() {
                    gst::warning!(gst::CAT_DEFAULT, "an input was already removed");
                }
            });
        if stopping.is_err() {
            gst::warning!(gst::CAT_DEFAULT, "could not stop an input");
        }
    }

    /// Stop `feed`'s attempt after `error` and retry after a delay.
    fn fail(&self, feed: &mut Feed, error: &str) {
        self.retire(feed);
        self.retire_outgoing(feed);
        feed.failures = feed.failures.saturating_add(1);
        let index = usize::try_from(feed.failures - 1).unwrap_or(usize::MAX);
        let delay = RETRY_DELAYS
            .get(index)
            .or(RETRY_DELAYS.last())
            .copied()
            .unwrap_or(1);
        feed.retry_at = Some(Instant::now() + Duration::from_secs(delay));
        match feed.role {
            Role::Main => {
                let mut status = lock(&self.status);
                "retrying".clone_into(&mut status.state);
                status.error = Some(error.to_owned());
                status.restarts = status.restarts.saturating_add(1);
            }
            Role::Standby => gst::warning!(gst::CAT_DEFAULT, "the standby failed: {error}"),
        }
    }

    /// Play `source` on `role` from now on, stopping what played.
    fn replace(&self, role: Role, source: &Prepared) {
        let mut feeds = lock(&self.feeds);
        if feeds.stopped {
            return;
        }
        let feed = Self::feed(&mut feeds, role);
        feed.source = source.clone();
        feed.failures = 0;
        feed.resolving = None;
        feed.retry_at = None;
        if role == Role::Main {
            *lock(&self.status) = SourceStatus::default();
        }
        self.hand_over(feed);
    }

    /// Convert every input to `canvas` while playing. Sources other than
    /// patterns keep playing and only change size, so a clip does not start
    /// over; patterns, and sources whose fill changed, restart.
    fn reshape(&self, canvas: Canvas) {
        let mut feeds = lock(&self.feeds);
        if feeds.stopped {
            return;
        }
        let previous = std::mem::replace(&mut *lock(&self.canvas), canvas);
        for role in [Role::Standby, Role::Main] {
            let feed = Self::feed(&mut feeds, role);
            self.retire_outgoing(feed);
            let Some(attempt) = &feed.attempt else {
                // A source waiting to retry starts with the new canvas.
                continue;
            };
            let kept = !feed.source.input.is_pattern()
                && previous.fill == canvas.fill
                && canvas.retarget(&attempt.bin).is_ok();
            if !kept {
                self.retire(feed);
                self.start(feed);
            }
        }
    }

    /// An input's stream ended: restart it, or leave the standby showing.
    fn ended(&self, src: Option<&gst::Object>) {
        let Some(src) = src else {
            return;
        };
        let mut feeds = lock(&self.feeds);
        for role in [Role::Main, Role::Standby] {
            let feed = Self::feed(&mut feeds, role);
            if feed.owned_by_outgoing(src) {
                // Its last frame stays until the next source's first, or
                // until it counts as stalled.
                return;
            }
            let Some(started) = feed.attempt.as_ref().map(|attempt| attempt.started) else {
                continue;
            };
            if !feed.owns(src) {
                continue;
            }
            if role == Role::Main && !self.restart.load(Ordering::Relaxed) {
                self.retire(feed);
                let mut status = lock(&self.status);
                "ended".clone_into(&mut status.state);
                status.error = None;
            } else if started.elapsed() < SHORTEST_RUN {
                self.fail(feed, "it ended right after starting");
            } else {
                if role == Role::Main {
                    let mut status = lock(&self.status);
                    status.restarts = status.restarts.saturating_add(1);
                }
                // The last frame stays until the restarted source's first.
                self.hand_over(feed);
            }
            return;
        }
    }

    /// `src` posted `error`: retry its input if it belongs to one. Returns
    /// whether an input, current or stopping, posted it.
    fn failed(&self, src: Option<&gst::Object>, error: &str) -> bool {
        let Some(src) = src else {
            return false;
        };
        let mut feeds = lock(&self.feeds);
        for role in [Role::Main, Role::Standby] {
            let feed = Self::feed(&mut feeds, role);
            if feed.owns(src) {
                self.fail(feed, error);
                return true;
            }
            if feed.owned_by_outgoing(src) {
                self.retire_outgoing(feed);
                return true;
            }
        }
        // Inputs being stopped may still report errors.
        std::iter::successors(Some(src.clone()), gst::Object::parent)
            .any(|object| object.name().starts_with(INPUT_PREFIX))
    }

    /// Retry what is due, find stalled inputs, and keep the slate current.
    fn tick(&self) {
        let mut feeds = lock(&self.feeds);
        if feeds.stopped {
            return;
        }
        for role in [Role::Standby, Role::Main] {
            self.check(Self::feed(&mut feeds, role));
        }
        let slate = self.slate_text(&feeds.main);
        if let Some(attempt) = feeds.standby.attempt.as_mut()
            && slate != attempt.slate
            && let Some(text) = attempt.bin.by_name(SLATE_TEXT)
        {
            text.set_property("text", &slate);
            attempt.slate = slate;
        }
    }

    fn check(&self, feed: &mut Feed) {
        // A source being replaced goes once the next one shows, or once it
        // has sent no frame for a while, as a clip that ended does.
        if let Some(outgoing) = &feed.outgoing {
            let mut selection = lock(&self.selection);
            let quiet = (*lock(&outgoing.last_frame)).map_or(Duration::MAX, |last| last.elapsed());
            if selection.showing == Showing::Outgoing && quiet > STALL_STANDBY {
                selection.stalled(&outgoing.pad);
            }
            let replaced = selection.showing != Showing::Outgoing;
            drop(selection);
            if replaced {
                self.retire_outgoing(feed);
            }
        }
        if feed.resolving.as_ref().is_some_and(JoinHandle::is_finished) {
            match feed.resolving.take().map(JoinHandle::join) {
                Some(Ok(Ok(source))) => {
                    feed.source = source;
                    self.start(feed);
                }
                Some(Ok(Err(error))) => self.fail(feed, &format!("{error:#}")),
                _ => self.fail(feed, "finding the stream failed"),
            }
        }
        if feed.retry_at.is_some_and(|at| at <= Instant::now()) {
            feed.retry_at = None;
            if feed.source.via == Via::YtDlp {
                // Streams found from web pages expire; find the stream again.
                let (source, resolver) = (feed.source.source.clone(), self.resolver.clone());
                match std::thread::Builder::new()
                    .name("wallplay-resolve".to_owned())
                    .spawn(move || Prepared::prepare(&source, &resolver))
                {
                    Ok(resolving) => feed.resolving = Some(resolving),
                    Err(error) => self.fail(feed, &error.to_string()),
                }
            } else {
                self.start(feed);
            }
        }
        let Some(attempt) = &feed.attempt else {
            return;
        };
        let last_frame = *lock(&attempt.last_frame);
        match last_frame {
            Some(last_frame) => {
                feed.failures = 0;
                if feed.role == Role::Main {
                    let mut status = lock(&self.status);
                    if status.state == "starting" {
                        "playing".clone_into(&mut status.state);
                        status.error = None;
                    }
                }
                let quiet = last_frame.elapsed();
                if quiet > STALL_RESTART {
                    self.fail(feed, &format!("no video for {} s", STALL_RESTART.as_secs()));
                } else if quiet > STALL_STANDBY {
                    lock(&self.selection).stalled(&attempt.pad);
                }
            }
            None if attempt.started.elapsed() > CONNECT_TIMEOUT => {
                self.fail(
                    feed,
                    &format!("no video within {} s", CONNECT_TIMEOUT.as_secs()),
                );
            }
            None => {}
        }
    }

    /// What the slate says about the source.
    fn slate_text(&self, main: &Feed) -> String {
        let name = main
            .source
            .title
            .clone()
            .unwrap_or_else(|| match &main.source.source {
                Source::Uri(url) | Source::Media(url) | Source::Page(url) | Source::Whep(url) => {
                    short_name(url)
                }
                Source::Pattern(_) | Source::Launch(_) => "The source".to_owned(),
            });
        let status = lock(&self.status);
        match (status.state.as_str(), &status.error) {
            ("ended", _) => format!("{name}\nended"),
            (_, Some(error)) => format!("{name}\n{error}\nRetrying"),
            _ => format!("{name}\nConnecting"),
        }
    }

    fn status(&self) -> SourceStatus {
        let standby = lock(&self.selection).showing == Showing::Standby;
        SourceStatus {
            standby,
            ..lock(&self.status).clone()
        }
    }

    /// Stop every input on this thread, for good.
    fn stop(&self) {
        let mut feeds = lock(&self.feeds);
        feeds.stopped = true;
        for role in [Role::Main, Role::Standby] {
            let feed = Self::feed(&mut feeds, role);
            feed.retry_at = None;
            feed.resolving = None;
            for attempt in feed.attempt.iter().chain(&feed.outgoing) {
                attempt.bin.set_locked_state(true);
                if attempt.bin.set_state(gst::State::Null).is_err() {
                    gst::warning!(gst::CAT_DEFAULT, "an input did not stop cleanly");
                }
            }
        }
    }
}

/// Everything a stamping probe needs.
struct Stamper {
    display: String,
    fps: u32,
    /// The current frames' layout and where their code goes, or `None`
    /// before the first caps and when frames are too small for a code.
    frames: Mutex<Option<(gst_video::VideoInfo, CodeGeometry)>>,
    measuring: Arc<AtomicBool>,
    /// Whether to draw the frame code, for browsers that cannot tell frames
    /// apart by their RTP timestamps.
    drawing: Arc<AtomicBool>,
    measurement: Arc<Mutex<Measurement>>,
    clock: Clock,
}

impl Stamper {
    /// Record when `warp`'s output for display `display` leaves it while
    /// measuring, drawing its frame code if asked.
    fn install(
        warp: &gst::Element,
        display: &str,
        fps: u32,
        (measuring, drawing): (&Arc<AtomicBool>, &Arc<AtomicBool>),
        measurement: &Arc<Mutex<Measurement>>,
        clock: Clock,
    ) -> Result<()> {
        let stamper = Self {
            display: display.to_owned(),
            fps,
            frames: Mutex::new(None),
            measuring: Arc::clone(measuring),
            drawing: Arc::clone(drawing),
            measurement: Arc::clone(measurement),
            clock,
        };
        let pad = warp
            .static_pad("src")
            .context("wallwarp has no source pad")?;
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |pad, info| {
                match info.data.as_mut() {
                    Some(gst::PadProbeData::Buffer(buffer)) => stamper.stamp(pad, buffer),
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Caps(caps) = event.view() {
                            stamper.follow(caps.caps());
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
        Ok(())
    }

    fn follow(&self, caps: &gst::CapsRef) {
        *lock(&self.frames) = gst_video::VideoInfo::from_caps(caps).ok().and_then(|info| {
            let code = CodeGeometry::for_output(info.width(), info.height())?;
            Some((info, code))
        });
    }

    fn stamp(&self, pad: &gst::Pad, buffer: &mut gst::Buffer) {
        if !self.measuring.load(Ordering::Relaxed) {
            return;
        }
        let frames = lock(&self.frames);
        let Some((info, code)) = frames.as_ref() else {
            return;
        };
        let Some(running) = buffer.pts().and_then(|pts| {
            let segment = pad.sticky_event::<gst::event::Segment>(0)?;
            segment
                .segment()
                .downcast_ref::<gst::format::Time>()?
                .to_running_time(pts)
        }) else {
            return;
        };
        let frame = frame_number(running, self.fps);
        // How far the pipeline clock has moved past the frame's running time.
        let delay = pad.parent_element().and_then(|element| {
            let now = element.clock()?.time();
            let late = now
                .checked_sub(element.base_time()?)?
                .checked_sub(running)?;
            Some(Duration::from(late).as_secs_f64() * 1000.0)
        });
        if self.drawing.load(Ordering::Relaxed) {
            let Ok(mut video) =
                gst_video::VideoFrameRef::from_buffer_ref_writable(buffer.make_mut(), info)
            else {
                return;
            };
            let stride = video
                .plane_stride()
                .first()
                .and_then(|stride| usize::try_from(*stride).ok());
            let (Some(stride), Ok(pixels)) = (stride, video.plane_data_mut(0)) else {
                return;
            };
            framecode::stamp(pixels, stride, code, frame);
        }
        lock(&self.measurement).stamped(&self.display, frame, self.clock.now_ms(), delay);
    }
}

impl Player {
    /// Build and start the pipeline for `layout`. `events` is called from a
    /// background thread.
    pub fn start(
        layout: &Layout,
        options: &PlayerOptions,
        measurement: &Arc<Mutex<Measurement>>,
        clock: Clock,
        events: impl Fn(PlayerEvent) + Send + 'static,
    ) -> Result<Self> {
        ensure!(options.fps > 0, "the frame rate must be positive");
        ensure!(!layout.screens.is_empty(), "the layout has no displays");
        check_elements(options)?;
        let pipeline = gst::Pipeline::with_name("wallplay");
        let measuring = Arc::new(AtomicBool::new(false));
        let drawing = Arc::new(AtomicBool::new(true));
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        let last_consumer = Arc::new(Mutex::new(None));
        // Inactive inputs drop their frames rather than wait for the active
        // one, which may have stalled.
        let selector = gst::ElementFactory::make("input-selector")
            .property("sync-streams", false)
            .build()?;
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()?;
        pipeline.add_many([&selector, &tee])?;
        selector.link(&tee)?;
        let sound = options.audio.then(|| Self::sound(&pipeline)).transpose()?;
        let (sound_selector, sound_tee) = match sound {
            Some((selector, pad, tee)) => (Some((selector, pad)), Some(tee)),
            None => (None, None),
        };
        let inputs = Arc::new(Inputs::new(
            &pipeline,
            selector,
            sound_selector,
            layout.canvas,
            options,
        )?);
        inputs.start_standby(&options.standby)?;
        let bus = pipeline.bus().context("the pipeline has no bus")?;
        let watched = Arc::clone(&inputs);
        let bus_thread = std::thread::Builder::new()
            .name("wallplay-bus".to_owned())
            .spawn(move || Self::watch(&bus, &watched, &events))?;
        let mut player = Self {
            pipeline,
            inputs,
            routes: Vec::new(),
            branches: Vec::new(),
            tee,
            sound_tee,
            generation,
            fps: options.fps,
            sink: options.sink.clone(),
            measuring,
            drawing,
            measurement: Arc::clone(measurement),
            clock,
            last_consumer,
            bus_thread: Some(bus_thread),
        };
        for screen in &layout.screens {
            player.add_display(layout, screen)?;
        }
        player
            .pipeline
            .set_state(gst::State::Playing)
            .context("starting the pipeline")?;
        // The source starts once the pipeline plays, so one that cannot
        // start, such as a missing file, is retried rather than failing it.
        let mut feeds = lock(&player.inputs.feeds);
        player.inputs.start(&mut feeds.main);
        drop(feeds);
        Ok(player)
    }

    /// Add `screen`'s branch to the pipeline, playing or not. The branch is
    /// linked from its end to its start, so frames find it ready.
    fn add_display(&mut self, layout: &Layout, screen: &Screen) -> Result<()> {
        let id = screen.id.as_str();
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 2_u32)
            .property("max-size-bytes", 0_u32)
            .property("max-size-time", 0_u64)
            .property_from_str("leaky", "downstream")
            .build()?;
        let warp = gst::ElementFactory::make("wallwarp")
            .name(format!("warp-{id}"))
            .property("quad", layout.source_quad(screen).to_string())
            .build()
            .context("creating wallwarp")?;
        let output = size_filter(output_caps(screen.output)?)?;
        let (sink, endpoint) = self.display_sink(id)?;
        // The warped output goes to the display and, a few times a second
        // and scaled down, to a preview for the control page.
        let split = make("tee")?;
        let to_sink = make("queue")?;
        let (preview, preview_filter) = Self::preview_branch(id, screen.output)?;
        let mut elements = vec![
            queue.clone(),
            warp.clone(),
            output.clone(),
            split.clone(),
            to_sink.clone(),
            sink.clone(),
        ];
        elements.extend(preview.iter().cloned());
        self.pipeline.add_many(&elements)?;
        gst::Element::link_many([&queue, &warp, &output, &split, &to_sink, &sink])?;
        split.link(preview.first().context("empty preview branch")?)?;
        gst::Element::link_many(&preview)?;
        if matches!(self.sink, Sink::WebRtc(_)) {
            allow_new_sizes(
                &to_sink
                    .static_pad("src")
                    .context("queue has no source pad")?,
            );
        }
        Stamper::install(
            &warp,
            id,
            self.fps,
            (&self.measuring, &self.drawing),
            &self.measurement,
            self.clock,
        )?;
        let pending = follow_canvas(&warp)?;
        let mut feeds = Vec::new();
        let mut sound_queue = None;
        if let Some(tee) = &self.sound_tee {
            let queue = make("queue")?;
            let input = Self::sound_input(&self.pipeline, id, &self.sink)?;
            self.pipeline.add(&queue)?;
            queue
                .static_pad("src")
                .context("queue has no source pad")?
                .link(&input)?;
            elements.insert(0, queue.clone());
            elements.extend(self.pipeline.by_name(&format!("sound-{id}")));
            sound_queue = Some((tee.clone(), queue));
        }
        for element in elements.iter().rev() {
            element.sync_state_with_parent()?;
        }
        let feed = |tee: &gst::Element, queue: &gst::Element| -> Result<gst::Pad> {
            let pad = tee
                .request_pad_simple("src_%u")
                .context("tee has no output to give")?;
            pad.link(&queue.static_pad("sink").context("queue has no sink pad")?)?;
            Ok(pad)
        };
        if let Some((tee, queue)) = &sound_queue {
            feeds.push(feed(tee, queue)?);
        }
        feeds.push(feed(&self.tee, &queue)?);
        self.branches.push(Branch {
            display: id.to_owned(),
            warp,
            pending,
            output,
            preview: preview_filter,
            feeds,
            elements,
        });
        self.routes.push(Route {
            display: id.to_owned(),
            output: screen.output,
            endpoint,
            generation: self.generation,
            code: CodeGeometry::for_output(screen.output.width, screen.output.height),
        });
        Ok(())
    }

    /// Take display `display` out of the pipeline, stopping its elements in
    /// the background: a `webrtcsink` can take its time, as [`Player::stop`]
    /// says, and the other displays keep playing meanwhile.
    fn remove_display(&mut self, display: &str) -> Result<()> {
        self.routes.retain(|route| route.display != display);
        let Some(index) = self
            .branches
            .iter()
            .position(|branch| branch.display == display)
        else {
            return Ok(());
        };
        let branch = self.branches.swap_remove(index);
        for pad in &branch.feeds {
            detach(pad)?;
        }
        self.pipeline.remove_many(&branch.elements)?;
        let wait = self.settle_time();
        std::thread::Builder::new()
            .name("wallplay-branch-teardown".to_owned())
            .spawn(move || {
                std::thread::sleep(wait);
                for element in &branch.elements {
                    if element.set_state(gst::State::Null).is_err() {
                        gst::warning!(gst::CAT_DEFAULT, "{} did not stop", element.name());
                    }
                }
            })?;
        Ok(())
    }

    /// How long until no session can still be negotiating.
    fn settle_time(&self) -> Duration {
        lock(&self.last_consumer).map_or(Duration::ZERO, |connected| {
            SETTLE.saturating_sub(connected.elapsed())
        })
    }

    /// Silence, the selector that picks between it and the inputs' sound,
    /// and the `tee` after it; returns the selector, where the silence joins
    /// it, and the `tee`.
    fn sound(pipeline: &gst::Pipeline) -> Result<(gst::Element, gst::Pad, gst::Element)> {
        // Paced like a clip rather than live: a live source would raise the
        // latency every sink waits for.
        let silence = gst::ElementFactory::make("audiotestsrc")
            .name("silence")
            .property_from_str("wave", "silence")
            .property("samplesperbuffer", AUDIO_SAMPLES)
            .build()?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", gst::Caps::from_str(AUDIO_CAPS)?)
            .build()?;
        let pace = make("clocksync")?;
        let selector = gst::ElementFactory::make("input-selector")
            .name("sound")
            .property("sync-streams", false)
            .build()?;
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()?;
        pipeline.add_many([&silence, &caps, &pace, &selector, &tee])?;
        gst::Element::link_many([&silence, &caps, &pace])?;
        selector.link(&tee)?;
        let pad = selector
            .request_pad_simple("sink_%u")
            .context("input-selector has no input to give")?;
        pace.static_pad("src")
            .context("clocksync has no source pad")?
            .link(&pad)?;
        selector.set_property("active-pad", &pad);
        Ok((selector, pad, tee))
    }

    /// Where display `id`'s sound goes: the audio input of its `webrtcsink`,
    /// named `sink-<id>`, or for tests an `appsink` named `sound-<id>`.
    fn sound_input(pipeline: &gst::Pipeline, id: &str, kind: &Sink) -> Result<gst::Pad> {
        match kind {
            Sink::WebRtc(_) => pipeline
                .by_name(&format!("sink-{id}"))
                .context("a display without its sink")?
                .request_pad_simple("audio_%u")
                .context("webrtcsink has no audio input to give"),
            Sink::AppSink => {
                let sink = gst::ElementFactory::make("appsink")
                    .name(format!("sound-{id}"))
                    .property("max-buffers", 5_u32)
                    .property("drop", true)
                    .property("sync", false)
                    .property("async", false)
                    .build()?;
                pipeline.add(&sink)?;
                sink.static_pad("sink").context("appsink has no sink pad")
            }
        }
    }

    /// Elements scaling a display's output down to a preview, ending in a
    /// `fakesink` named `preview-<id>` that keeps the latest frame; returns
    /// them and the `capsfilter` giving the preview its size.
    fn preview_branch(id: &str, output: Size) -> Result<(Vec<gst::Element>, gst::Element)> {
        let filter = size_filter(preview_caps(output))?;
        let elements = vec![
            gst::ElementFactory::make("queue")
                .property("max-size-buffers", 1_u32)
                .property_from_str("leaky", "downstream")
                .build()?,
            gst::ElementFactory::make("videorate")
                .property("drop-only", true)
                .property("max-rate", PREVIEW_RATE)
                .build()?,
            make("videoscale")?,
            filter.clone(),
            gst::ElementFactory::make("fakesink")
                .name(format!("preview-{id}"))
                .property("sync", false)
                .property("async", false)
                .property("enable-last-sample", true)
                .build()?,
        ];
        Ok((elements, filter))
    }

    /// The sink of display `id`, on a signalling port no route has, noting
    /// when a browser connects to it.
    fn display_sink(&self, id: &str) -> Result<(gst::Element, Endpoint)> {
        let taken: Vec<u16> = self
            .routes
            .iter()
            .filter_map(|route| match route.endpoint {
                Endpoint::Port(port) => Some(port),
                _ => None,
            })
            .collect();
        let (sink, endpoint) = Self::sink(id, self.generation, &taken, &self.sink)?;
        if matches!(endpoint, Endpoint::Port(_) | Endpoint::Uri(_)) {
            let last_consumer = Arc::clone(&self.last_consumer);
            sink.connect("consumer-added", false, move |_| {
                *lock(&last_consumer) = Some(Instant::now());
                None
            });
        }
        Ok((sink, endpoint))
    }

    fn sink(
        id: &str,
        generation: u32,
        taken: &[u16],
        sink: &Sink,
    ) -> Result<(gst::Element, Endpoint)> {
        match sink {
            Sink::AppSink => {
                let sink = gst::ElementFactory::make("appsink")
                    .name(format!("sink-{id}"))
                    .property("max-buffers", 4_u32)
                    .property("drop", true)
                    .property("sync", false)
                    .property("async", false)
                    .build()?;
                Ok((sink, Endpoint::None))
            }
            Sink::WebRtc(signalling) => {
                let sink = gst::ElementFactory::make("webrtcsink")
                    .name(format!("sink-{id}"))
                    .property(
                        "meta",
                        gst::Structure::builder("meta")
                            .field("display", id)
                            .field("generation", generation)
                            .build(),
                    )
                    // Local networks need no STUN, and an unreachable default
                    // server only slows connections down.
                    .property("stun-server", None::<String>)
                    // Keep every display's resolution and frame rate; a wall
                    // with one blurry or stuttering display looks broken.
                    .property_from_str("enable-mitigation-modes", "none")
                    // VP8 plays in every current browser and encodes on every
                    // platform. H.264 depends on the platform's encoders: on
                    // macOS, webrtcsink's codec discovery rejects
                    // vtenc_h264's output, failing the whole pipeline.
                    .property("video-caps", gst::Caps::from_str("video/x-vp8")?)
                    .build()?;
                // RTP timestamps then follow the running time, so browsers
                // can tell frames apart without a code drawn into them.
                sink.connect("payloader-setup", false, |values| {
                    if let Some(payloader) = values
                        .get(3)
                        .and_then(|value| value.get::<gst::Element>().ok())
                        && payloader.has_property("timestamp-offset")
                    {
                        payloader.set_property("timestamp-offset", 0_u32);
                    }
                    // webrtcsink still applies its own settings.
                    Some(false.to_value())
                });
                let endpoint = match signalling {
                    Signalling::BuiltIn { host, first_port } => {
                        let port = free_port(host, *first_port, taken).with_context(|| {
                            format!("no free signalling port on {host} from {first_port}")
                        })?;
                        sink.set_property("run-signalling-server", true);
                        sink.set_property("signalling-server-host", host);
                        sink.set_property("signalling-server-port", u32::from(port));
                        Endpoint::Port(port)
                    }
                    Signalling::External(uri) => {
                        sink.property::<gst::glib::Object>("signaller")
                            .set_property("uri", uri);
                        Endpoint::Uri(uri.clone())
                    }
                };
                Ok((sink, endpoint))
            }
        }
    }

    fn watch(bus: &gst::Bus, inputs: &Inputs, events: &impl Fn(PlayerEvent)) {
        let mut ticked = Instant::now();
        while !inputs.halted.load(Ordering::Relaxed) {
            let message = bus.timed_pop(gst::ClockTime::try_from(TICK).ok());
            match message.as_deref().map(gst::MessageRef::view) {
                Some(gst::MessageView::Application(application)) => {
                    match application.structure().map(gst::StructureRef::name) {
                        Some(name) if name == STOP_MESSAGE => return,
                        Some(name) if name == ENDED_MESSAGE => inputs.ended(application.src()),
                        _ => {}
                    }
                }
                Some(gst::MessageView::StateChanged(change))
                    if change
                        .src()
                        .is_some_and(|src| src.type_() == gst::Pipeline::static_type())
                        && change.current() == gst::State::Playing =>
                {
                    events(PlayerEvent::Playing);
                }
                Some(gst::MessageView::Eos(_)) => events(PlayerEvent::Finished),
                Some(gst::MessageView::Error(error))
                    if !inputs.failed(error.src(), &input_error(error)) =>
                {
                    let source = error
                        .src()
                        .map(|src| format!("{}: ", src.name()))
                        .unwrap_or_default();
                    let details = error
                        .debug()
                        .map(|debug| format!(" ({debug})"))
                        .unwrap_or_default();
                    events(PlayerEvent::Failed(format!(
                        "{source}{}{details}",
                        error.error()
                    )));
                }
                _ => {}
            }
            if ticked.elapsed() >= TICK {
                ticked = Instant::now();
                inputs.tick();
            }
        }
    }

    /// Play `layout`, filling its canvas as `fill` says, without restarting
    /// the pipeline: displays that stay keep their streams, also through a
    /// change of size, and displays leave and join. Returns whether that
    /// worked; otherwise the player needs a restart.
    #[must_use = "when false, the player needs a restart to play the layout"]
    pub fn update_layout(&mut self, layout: &Layout, fill: Fill) -> bool {
        match self.apply_layout(layout, fill) {
            Ok(()) => true,
            Err(error) => {
                gst::warning!(gst::CAT_DEFAULT, "could not change the layout: {error:#}");
                false
            }
        }
    }

    fn apply_layout(&mut self, layout: &Layout, fill: Fill) -> Result<()> {
        ensure!(!layout.screens.is_empty(), "the layout has no displays");
        let leaving: Vec<String> = self
            .routes
            .iter()
            .map(|route| route.display.clone())
            .filter(|display| layout.screen(display).is_none())
            .collect();
        for display in &leaving {
            self.remove_display(display)?;
        }
        let canvas = Canvas {
            size: layout.canvas,
            fill,
            ..*lock(&self.inputs.canvas)
        };
        let resized = lock(&self.inputs.canvas).size != canvas.size;
        for screen in &layout.screens {
            let Some(branch) = self
                .branches
                .iter()
                .find(|branch| branch.display == screen.id)
            else {
                self.add_display(layout, screen)?;
                continue;
            };
            let quad = layout.source_quad(screen).to_string();
            {
                let mut pending = lock(&branch.pending);
                if resized {
                    *pending = Some((canvas.size, quad));
                } else {
                    *pending = None;
                    drop(pending);
                    branch.warp.set_property("quad", quad);
                }
            }
            if let Some(route) = self
                .routes
                .iter_mut()
                .find(|route| route.display == screen.id)
                && route.output != screen.output
            {
                branch
                    .preview
                    .set_property("caps", preview_caps(screen.output));
                branch
                    .output
                    .set_property("caps", output_caps(screen.output)?);
                route.output = screen.output;
                route.code = CodeGeometry::for_output(screen.output.width, screen.output.height);
            }
        }
        self.routes.sort_by_key(|route| {
            layout
                .screens
                .iter()
                .position(|screen| screen.id == route.display)
        });
        if *lock(&self.inputs.canvas) != canvas {
            self.inputs.reshape(canvas);
        }
        Ok(())
    }

    /// Play `source` from now on, without restarting the pipeline: the
    /// displays keep their streams and show the standby until it plays.
    pub fn set_source(&self, source: &Prepared) {
        self.inputs.replace(Role::Main, source);
    }

    /// Show pattern `id` whenever the source is not showing.
    pub fn set_standby(&self, id: &str) -> Result<()> {
        self.inputs.replace(Role::Standby, &Prepared::pattern(id)?);
        Ok(())
    }

    /// Whether a source that ends starts again.
    pub fn set_restart(&self, restart: bool) {
        self.inputs.restart.store(restart, Ordering::Relaxed);
    }

    #[must_use]
    pub fn source_status(&self) -> SourceStatus {
        self.inputs.status()
    }

    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    #[must_use]
    pub fn route(&self, display: &str) -> Option<&Route> {
        self.routes.iter().find(|route| route.display == display)
    }

    /// Start or stop recording when frames leave the pipeline, drawing their
    /// codes as [`Player::set_codes`] says.
    pub fn set_measuring(&self, measuring: bool) {
        self.measuring.store(measuring, Ordering::Relaxed);
    }

    /// Whether measuring draws frame codes, which browsers need unless they
    /// tell frames apart by RTP timestamp. On until told otherwise.
    pub fn set_codes(&self, drawing: bool) {
        self.drawing.store(drawing, Ordering::Relaxed);
    }

    #[must_use]
    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    /// The element keeping display `display`'s latest preview frame.
    #[must_use]
    pub fn preview_sink(&self, display: &str) -> Option<gst::Element> {
        self.pipeline.by_name(&format!("preview-{display}"))
    }

    /// Stop playback on another thread, waiting at most `timeout`, and
    /// return whether it stopped.
    ///
    /// `webrtcsink` 0.15.4 can deadlock tearing down a session whose remote
    /// description is still being applied, as when a browser has just
    /// connected, and then never finishes stopping. Teardown therefore waits
    /// until [`SETTLE`] has passed since the last browser connected. Measuring
    /// and the inputs stop first, so a pipeline that gets stuck anyway neither
    /// stamps frames nor keeps warping; its threads and signalling ports stay
    /// in use until the process exits.
    #[must_use]
    pub fn stop(self, timeout: Duration) -> bool {
        self.set_measuring(false);
        self.inputs.stop();
        let (stopped, wait) = mpsc::channel();
        let teardown = std::thread::Builder::new()
            .name("wallplay-teardown".to_owned())
            .spawn(move || {
                std::thread::sleep(self.settle_time());
                drop(self);
                // The caller may have stopped waiting.
                if stopped.send(()).is_err() {
                    gst::info!(gst::CAT_DEFAULT, "a pipeline stopped after its timeout");
                }
            });
        teardown.is_ok() && wait.recv_timeout(timeout).is_ok()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The bus thread notices within a tick; the message, posted before
        // stopping makes the bus drop messages, wakes it at once.
        self.inputs.halted.store(true, Ordering::Relaxed);
        let stop = gst::message::Application::new(gst::Structure::new_empty(STOP_MESSAGE));
        if self
            .pipeline
            .bus()
            .is_none_or(|bus| bus.post(stop).is_err())
        {
            gst::info!(gst::CAT_DEFAULT, "could not wake the bus thread");
        }
        if self.pipeline.set_state(gst::State::Null).is_err() {
            gst::warning!(gst::CAT_DEFAULT, "the pipeline did not stop cleanly");
        }
        if let Some(thread) = self.bus_thread.take()
            && thread.join().is_err()
        {
            gst::warning!(gst::CAT_DEFAULT, "the bus thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slates_name_urls_briefly() {
        assert_eq!(short_name("file:///videos/wall/clip.mp4"), "clip.mp4");
        assert_eq!(short_name("rtsp://camera.local:554/"), "camera.local:554");
        assert_eq!(
            short_name("https://h/live/index.m3u8?token=x"),
            "index.m3u8"
        );
        assert_eq!(
            short_name("srt://10.0.0.5:7001?streamid=read:cam"),
            "10.0.0.5:7001"
        );
    }

    #[test]
    fn covering_crops_trim_the_longer_edges() {
        gst::init().expect("gst");
        let frame = |w, h, par: (i32, i32)| {
            gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, w, h)
                .par(gst::Fraction::new(par.0, par.1))
                .build()
                .expect("info")
        };
        let rows = |cut| Sides {
            top: cut,
            bottom: cut,
            ..Sides::default()
        };
        let columns = |cut| Sides {
            left: cut,
            right: cut,
            ..Sides::default()
        };
        // The race this replaces: 640x360 frames for a 640x300 canvas.
        assert_eq!(
            Sides::covering(&frame(640, 360, (1, 1)), 640, 300),
            rows(30)
        );
        assert_eq!(
            Sides::covering(&frame(640, 300, (1, 1)), 640, 300),
            Sides::default()
        );
        assert_eq!(
            Sides::covering(&frame(1920, 1080, (1, 1)), 1080, 1920),
            columns(656)
        );
        // Wide pixels make a wider picture: 704 of 720 columns are 16:9.
        assert_eq!(
            Sides::covering(&frame(720, 576, (16, 11)), 16, 9),
            columns(8)
        );
        assert_eq!(Sides::covering(&frame(1000, 2, (1, 1)), 1, 1), columns(499));
        // Nothing would be left of a 3x1 frame on a 1:3 canvas.
        assert_eq!(
            Sides::covering(&frame(3, 1, (1, 1)), 1, 3),
            Sides::default()
        );
    }

    #[test]
    fn running_times_round_to_frames() {
        let frame = |ms| frame_number(gst::ClockTime::from_mseconds(ms), 30);
        assert_eq!(frame(0), 0);
        assert_eq!(frame(33), 1);
        assert_eq!(frame(1000), 30);
    }
}
