use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use wall_geometry::{Homography, Quad};

use super::Interpolation;
use super::warp::{Plane, PlaneMut, Warp};

const DEFAULT_QUAD: &str = "0,0,1,0,1,1,0,1";
const DEFAULT_OUTPUT_SIZE: u32 = 0;
const DEFAULT_BACKGROUND: u32 = 0xff00_0000;
const DEFAULT_THREADS: u32 = 1;
const MAX_OUTPUT_SIZE: u32 = i32::MAX.unsigned_abs();

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "wallwarp",
        gst::DebugColorFlags::empty(),
        Some("Video wall perspective warp"),
    )
});

struct Settings {
    quad_text: String,
    /// The validated quadrilateral, or why `quad_text` is unusable.
    quad: Result<Quad, String>,
    output_width: u32,
    output_height: u32,
    interpolation: Interpolation,
    background: u32,
    threads: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            quad_text: DEFAULT_QUAD.to_owned(),
            quad: Ok(Quad::UNIT),
            output_width: DEFAULT_OUTPUT_SIZE,
            output_height: DEFAULT_OUTPUT_SIZE,
            interpolation: Interpolation::default(),
            background: DEFAULT_BACKGROUND,
            threads: DEFAULT_THREADS,
        }
    }
}

/// The settings in use while streaming.
#[derive(Clone, Copy)]
struct Config {
    quad: Quad,
    local_to_source: Homography,
    interpolation: Interpolation,
    background: [u8; 4],
    /// Most threads warping one frame.
    threads: usize,
}

/// What caps negotiation settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Negotiated {
    source: (u32, u32),
    output: (u32, u32),
    /// Whether the input and output video info are equal.
    same_info: bool,
}

#[derive(Default)]
struct State {
    config: Option<Config>,
    /// Coefficients for the negotiated frame sizes.
    warp: Option<Warp>,
    negotiated: Option<Negotiated>,
    /// Counts changes of `config`, so that coefficients located for an older
    /// one are not installed.
    revision: u64,
}

#[derive(Default)]
pub struct WallWarp {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn parse_quad(text: &str) -> Result<Quad, String> {
    let quad = text.parse::<Quad>().map_err(|err| err.to_string())?;
    Homography::unit_square_to_quad(&quad).map_err(|err| err.to_string())?;
    Ok(quad)
}

/// Big-endian ARGB to RGBA bytes.
fn rgba(argb: u32) -> [u8; 4] {
    let [a, r, g, b] = argb.to_be_bytes();
    [r, g, b, a]
}

/// A fixed dimension, or any dimension when `value` is zero.
fn set_dimension(structure: &mut gst::StructureRef, field: &str, value: u32) {
    match i32::try_from(value) {
        Ok(value) if value > 0 => structure.set(field, value),
        _ => structure.set(field, gst::IntRange::new(1, i32::MAX)),
    }
}

fn plane_stride(frame: &impl VideoFrameExt) -> Result<usize, &'static str> {
    frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
        .ok_or("negative plane strides are not supported")
}

/// The threads warping a frame for the `n-threads` value `threads`.
fn thread_count(threads: u32) -> usize {
    match threads {
        0 => std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        threads => usize::try_from(threads).unwrap_or(usize::MAX),
    }
}

impl WallWarp {
    /// Take a new quad. While streaming, an invalid one is ignored, and a
    /// valid one replaces the coefficients between frames.
    fn set_quad(&self, text: String) {
        let quad = parse_quad(&text);
        let streaming = lock(&self.state).config.is_some();
        if let Err(err) = &quad {
            let action = if streaming {
                "; keeping the current one"
            } else {
                ""
            };
            gst::warning!(CAT, imp = self, "invalid quad {text:?}: {err}{action}");
            if streaming {
                return;
            }
        }
        {
            let mut settings = lock(&self.settings);
            settings.quad.clone_from(&quad);
            settings.quad_text = text;
        }
        let Ok(quad) = quad else {
            return;
        };
        let Ok(local_to_source) = Homography::unit_square_to_quad(&quad) else {
            return;
        };
        self.reconfigure(|config| {
            config.quad = quad;
            config.local_to_source = local_to_source;
        });
    }

    /// Change the settings in use, if streaming; new coefficients replace the
    /// old ones between frames.
    fn reconfigure(&self, change: impl FnOnce(&mut Config)) {
        let (config, negotiated, revision) = {
            let mut state = lock(&self.state);
            let Some(config) = state.config.as_mut() else {
                return;
            };
            change(config);
            let config = *config;
            state.revision += 1;
            (config, state.negotiated, state.revision)
        };
        let Some(Negotiated {
            source,
            output,
            same_info,
        }) = negotiated
        else {
            return;
        };
        match Warp::new(
            &config.local_to_source,
            source,
            output,
            config.interpolation,
            config.background,
        ) {
            Ok(warp) => {
                let mut state = lock(&self.state);
                // A renegotiation or another change meanwhile located its own
                // samples.
                let current = state.negotiated == negotiated && state.revision == revision;
                if current {
                    state.warp = Some(warp);
                }
                drop(state);
                if current {
                    self.obj()
                        .set_passthrough(config.quad == Quad::UNIT && same_info);
                }
            }
            Err(err) => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "cannot warp {source:?} to {output:?}: {err}"
                );
            }
        }
    }

    fn map_flow_error(&self, err: impl std::fmt::Display) -> gst::FlowError {
        gst::element_imp_error!(self, gst::StreamError::Failed, ["{err}"]);
        gst::FlowError::Error
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WallWarp {
    const NAME: &'static str = "GstSmithWallWarp";
    type Type = super::WallWarp;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for WallWarp {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> =
            LazyLock::new(|| {
                vec![
                glib::ParamSpecString::builder("quad")
                    .nick("Quadrilateral")
                    .blurb(
                        "Source region shown by the output: normalized x,y of the source points \
                         at the output top-left, top-right, bottom-right, and bottom-left corners \
                         (0,0 is the source top-left, 1,1 its bottom-right)",
                    )
                    .default_value(Some(DEFAULT_QUAD))
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("output-width")
                    .nick("Output width")
                    .blurb("Output width in pixels; 0 negotiates it, preferring the input width")
                    .maximum(MAX_OUTPUT_SIZE)
                    .default_value(DEFAULT_OUTPUT_SIZE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("output-height")
                    .nick("Output height")
                    .blurb("Output height in pixels; 0 negotiates it, preferring the input height")
                    .maximum(MAX_OUTPUT_SIZE)
                    .default_value(DEFAULT_OUTPUT_SIZE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("interpolation", Interpolation::default())
                    .nick("Interpolation")
                    .blurb("How output pixels sample the source")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("background")
                    .nick("Background")
                    .blurb(
                        "Color of output pixels that sample outside the source, as big-endian \
                         ARGB (0xAARRGGBB)",
                    )
                    .default_value(DEFAULT_BACKGROUND)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("n-threads")
                    .nick("Threads")
                    .blurb("Maximum number of threads warping each frame; 0 uses one per CPU core")
                    .default_value(DEFAULT_THREADS)
                    .mutable_playing()
                    .build(),
            ]
            });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "quad" => {
                let text = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                self.set_quad(text);
            }
            // The next frame renegotiates with downstream.
            "output-width" => {
                lock(&self.settings).output_width = value.get().unwrap_or(DEFAULT_OUTPUT_SIZE);
                self.obj().reconfigure_src();
            }
            "output-height" => {
                lock(&self.settings).output_height = value.get().unwrap_or(DEFAULT_OUTPUT_SIZE);
                self.obj().reconfigure_src();
            }
            "interpolation" => {
                let interpolation = value.get().unwrap_or_default();
                lock(&self.settings).interpolation = interpolation;
                self.reconfigure(|config| config.interpolation = interpolation);
            }
            "background" => {
                let background = value.get().unwrap_or(DEFAULT_BACKGROUND);
                lock(&self.settings).background = background;
                self.reconfigure(|config| config.background = rgba(background));
            }
            "n-threads" => {
                let threads = value.get().unwrap_or(DEFAULT_THREADS);
                lock(&self.settings).threads = threads;
                self.reconfigure(|config| config.threads = thread_count(threads));
            }
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "quad" => settings.quad_text.to_value(),
            "output-width" => settings.output_width.to_value(),
            "output-height" => settings.output_height.to_value(),
            "interpolation" => settings.interpolation.to_value(),
            "background" => settings.background.to_value(),
            "n-threads" => settings.threads.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for WallWarp {}

impl ElementImpl for WallWarp {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Video wall warp",
                "Filter/Converter/Video/Scaler",
                "Maps a quadrilateral region of the input onto the whole output frame through a \
                 perspective transform, producing the stream for one video wall display",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .build();
            [
                ("src", gst::PadDirection::Src),
                ("sink", gst::PadDirection::Sink),
            ]
            .into_iter()
            .filter_map(|(name, direction)| {
                gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps).ok()
            })
            .collect()
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for WallWarp {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    // Equal caps can still describe a non-identity warp; set_info decides.
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let config = {
            let settings = lock(&self.settings);
            let invalid = |err: &dyn std::fmt::Display| {
                gst::error_msg!(
                    gst::LibraryError::Settings,
                    ("invalid quad {:?}: {err}", settings.quad_text)
                )
            };
            let quad = settings.quad.as_ref().map_err(|err| invalid(err))?;
            let local_to_source =
                Homography::unit_square_to_quad(quad).map_err(|err| invalid(&err))?;
            Config {
                quad: *quad,
                local_to_source,
                interpolation: settings.interpolation,
                background: rgba(settings.background),
                threads: thread_count(settings.threads),
            }
        };
        *lock(&self.state) = State {
            config: Some(config),
            ..State::default()
        };
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *lock(&self.state) = State::default();
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        // Output sizes come from the properties or downstream; input sizes are free.
        let (width, height) = match direction {
            gst::PadDirection::Sink => {
                let settings = lock(&self.settings);
                (settings.output_width, settings.output_height)
            }
            _ => (0, 0),
        };
        let mut other = gst::Caps::new_empty();
        {
            let other = other.get_mut()?;
            for (structure, features) in caps.iter_with_features() {
                let mut structure = structure.to_owned();
                set_dimension(&mut structure, "width", width);
                set_dimension(&mut structure, "height", height);
                // Display pixels need not share the source pixel aspect ratio.
                structure.remove_field("pixel-aspect-ratio");
                other.append_structure_full(structure, Some(features.to_owned()));
            }
        }
        gst::trace!(
            CAT,
            imp = self,
            "transformed {caps} in direction {direction:?} to {other}"
        );
        Some(match filter {
            Some(filter) => filter.intersect_with_mode(&other, gst::CapsIntersectMode::First),
            None => other,
        })
    }

    fn fixate_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        mut othercaps: gst::Caps,
    ) -> gst::Caps {
        if direction != gst::PadDirection::Sink {
            return self.parent_fixate_caps(direction, caps, othercaps);
        }
        // Prefer the input size and square pixels when downstream allows a range.
        othercaps.truncate();
        if let Some(input) = caps.structure(0)
            && let Some(output) = othercaps.make_mut().structure_mut(0)
        {
            for field in ["width", "height"] {
                if let Ok(value) = input.get::<i32>(field) {
                    output.fixate_field_nearest_int(field, value);
                }
            }
            if let Ok(framerate) = input.get::<gst::Fraction>("framerate") {
                output.fixate_field_nearest_fraction("framerate", framerate);
            }
            let square = gst::Fraction::new(1, 1);
            if output.has_field("pixel-aspect-ratio") {
                output.fixate_field_nearest_fraction("pixel-aspect-ratio", square);
            } else {
                output.set("pixel-aspect-ratio", square);
            }
        }
        othercaps.fixate();
        othercaps
    }
}

impl VideoFilterImpl for WallWarp {
    fn set_info(
        &self,
        _incaps: &gst::Caps,
        in_info: &gst_video::VideoInfo,
        _outcaps: &gst::Caps,
        out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        let mut state = lock(&self.state);
        let Some(config) = state.config.as_ref() else {
            return Err(gst::loggable_error!(
                CAT,
                "caps set before the element started"
            ));
        };
        let source = (in_info.width(), in_info.height());
        let output = (out_info.width(), out_info.height());
        let warp = Warp::new(
            &config.local_to_source,
            source,
            output,
            config.interpolation,
            config.background,
        )
        .map_err(|err| gst::loggable_error!(CAT, "cannot warp {source:?} to {output:?}: {err}"))?;
        // The identity warp copies every pixel exactly, so skip the copy.
        let passthrough = config.quad == Quad::UNIT && in_info == out_info;
        state.negotiated = Some(Negotiated {
            source,
            output,
            same_info: in_info == out_info,
        });
        gst::debug!(
            CAT,
            imp = self,
            "warping {}x{} to {}x{} (passthrough: {passthrough})",
            source.0,
            source.1,
            output.0,
            output.1
        );
        state.warp = Some(warp);
        drop(state);
        self.obj().set_passthrough(passthrough);
        Ok(())
    }

    fn transform_frame(
        &self,
        inframe: &gst_video::VideoFrameRef<&gst::BufferRef>,
        outframe: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = lock(&self.state);
        let (Some(warp), Some(config)) = (state.warp.as_ref(), state.config.as_ref()) else {
            gst::element_imp_error!(self, gst::CoreError::Negotiation, ["no negotiated caps"]);
            return Err(gst::FlowError::NotNegotiated);
        };
        let source = Plane::new(
            inframe
                .plane_data(0)
                .map_err(|err| self.map_flow_error(err))?,
            plane_stride(inframe).map_err(|err| self.map_flow_error(err))?,
            inframe.width(),
            inframe.height(),
        );
        let stride = plane_stride(outframe).map_err(|err| self.map_flow_error(err))?;
        let (width, height) = (outframe.width(), outframe.height());
        let mut target = PlaneMut::new(
            outframe
                .plane_data_mut(0)
                .map_err(|err| self.map_flow_error(err))?,
            stride,
            width,
            height,
        );
        warp.apply(&source, &mut target, config.threads)
            .map_err(|err| self.map_flow_error(err))?;
        Ok(gst::FlowSuccess::Ok)
    }
}
