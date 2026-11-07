use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use wall_geometry::{Homography, Quad};

use crate::wallwarp::{Interpolation, parse_quad, rgba};

const DEFAULT_QUAD: &str = "0,0,1,0,1,1,0,1";
const DEFAULT_OUTPUT_SIZE: u32 = 0;
const DEFAULT_BACKGROUND: u32 = 0xff00_0000;
const MAX_OUTPUT_SIZE: u32 = i32::MAX.unsigned_abs();

/// Samples the source as `wallwarp` does: `h0`…`h8` map the output's
/// `[0, 1]²` to the normalized source row by row, every output pixel samples
/// at its center, and a sample outside the source or behind the projection
/// shows the background. The texture's linear filtering blends the four
/// nearest source pixels, clamped to the edge; nearest sampling moves the
/// sample to the center of the source pixel it falls in.
///
/// Single precision lands a sample that falls exactly on a pixel boundary,
/// as scaling 48 rows to 30 does every fifth row, a little short of it. Its
/// error grows with the coordinate, so the position is nudged by 2⁻¹⁹ of the
/// source size, a few times that error, to pick the pixel exact arithmetic
/// picks, there and at the source's edges.
///
/// GLES 2 devices without high precision in fragment shaders get medium
/// precision, too coarse for the nudge: boundary samples may take a
/// neighbour there.
const SHADER: &str = "#version 100
#ifdef GL_ES
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;
uniform float h0, h1, h2, h3, h4, h5, h6, h7, h8;
uniform float source_width, source_height, nearest;
uniform float background_red, background_green, background_blue, background_alpha;
void main () {
  vec2 o = v_texcoord;
  float w = h6 * o.x + h7 * o.y + h8;
  vec2 s = vec2(h0 * o.x + h1 * o.y + h2, h3 * o.x + h4 * o.y + h5) / w;
  vec2 size = vec2(source_width, source_height);
  vec2 p = s * size + size / 524288.0;
  if (w <= 0.0 || p.x < 0.0 || p.y < 0.0 || p.x >= size.x || p.y >= size.y) {
    gl_FragColor = vec4(background_red, background_green, background_blue, background_alpha);
  } else if (nearest > 0.5) {
    gl_FragColor = texture2D(tex, (floor(p) + 0.5) / size);
  } else {
    gl_FragColor = texture2D(tex, s);
  }
}
";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "glwallwarp",
        gst::DebugColorFlags::empty(),
        Some("Video wall perspective warp in OpenGL"),
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
        }
    }
}

/// What the shader draws with.
struct Drawing {
    /// The last valid quad: an invalid one set while streaming is ignored.
    quad: Quad,
    /// The negotiated input size, in which the shader finds source pixels.
    source: (u32, u32),
}

impl Default for Drawing {
    fn default() -> Self {
        Self {
            quad: Quad::UNIT,
            source: (1, 1),
        }
    }
}

/// The children whose settings follow the properties.
struct Children {
    shader: gst::Element,
    /// The `capsfilter` giving the shader's output size.
    size: gst::Element,
}

#[derive(Default)]
pub struct GlWallWarp {
    settings: Mutex<Settings>,
    drawing: Mutex<Drawing>,
    /// The children, or why they could not be made.
    children: OnceLock<Result<Children, String>>,
    streaming: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The shader's output caps for an output size, where zero leaves a
/// dimension to negotiation. Square pixels come first, so they are preferred
/// where downstream allows them, as `wallwarp` prefers them.
fn size_caps(width: u32, height: u32) -> gst::Caps {
    let mut caps = gst::Caps::from_str(
        "video/x-raw(memory:GLMemory),format=RGBA,pixel-aspect-ratio=1/1; \
         video/x-raw(memory:GLMemory),format=RGBA",
    )
    .unwrap_or_else(|_err| gst::Caps::new_any());
    for structure in caps.make_mut().iter_mut() {
        for (field, value) in [("width", width), ("height", height)] {
            if let Ok(value) = i32::try_from(value)
                && value > 0
            {
                structure.set(field, value);
            }
        }
    }
    caps
}

/// The shader's uniforms for a quad, source size, sampling and background.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the shader computes in single precision"
)]
fn uniforms(
    quad: &Quad,
    source: (u32, u32),
    interpolation: Interpolation,
    background: u32,
) -> gst::Structure {
    let mut uniforms = gst::Structure::builder("uniforms");
    if let Ok(homography) = Homography::unit_square_to_quad(quad) {
        for (index, value) in homography.matrix().iter().flatten().enumerate() {
            uniforms = uniforms.field(format!("h{index}"), *value as f32);
        }
    }
    let [red, green, blue, alpha] = rgba(background);
    let nearest = if interpolation == Interpolation::Nearest {
        1.0_f32
    } else {
        0.0
    };
    uniforms
        .field("source_width", f64::from(source.0) as f32)
        .field("source_height", f64::from(source.1) as f32)
        .field("nearest", nearest)
        .field("background_red", f32::from(red) / 255.0)
        .field("background_green", f32::from(green) / 255.0)
        .field("background_blue", f32::from(blue) / 255.0)
        .field("background_alpha", f32::from(alpha) / 255.0)
        .build()
}

impl GlWallWarp {
    fn children(&self) -> Result<&Children, &str> {
        match self.children.get() {
            Some(Ok(children)) => Ok(children),
            Some(Err(err)) => Err(err),
            None => Err("the element was not constructed"),
        }
    }

    /// Add and link the children, returning those the properties drive.
    fn build(&self) -> Result<Children, String> {
        let make = |factory: &str| {
            gst::ElementFactory::make(factory).build().map_err(|_err| {
                format!("no {factory} element; is the GStreamer OpenGL plugin installed?")
            })
        };
        let capsfilter = |caps: &str| {
            let caps = gst::Caps::from_str(caps).map_err(|err| err.to_string())?;
            make("capsfilter").inspect(|filter| filter.set_property("caps", caps))
        };
        let upload = make("glupload")?;
        let convert = make("glcolorconvert")?;
        // The shader samples an RGBA 2D texture.
        let texture = capsfilter("video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D")?;
        let shader = make("glshader")?;
        shader.set_property("fragment", SHADER);
        let size = capsfilter("video/x-raw(memory:GLMemory)")?;
        let finish = make("glcolorconvert")?;
        let download = make("gldownload")?;
        let chain = [
            &upload, &convert, &texture, &shader, &size, &finish, &download,
        ];
        let obj = self.obj();
        obj.add_many(chain).map_err(|err| err.to_string())?;
        gst::Element::link_many(chain).map_err(|err| err.to_string())?;
        for (name, child) in [("sink", &upload), ("src", &download)] {
            let target = child
                .static_pad(name)
                .ok_or_else(|| format!("no {name} pad"))?;
            obj.static_pad(name)
                .and_then(|pad| pad.downcast::<gst::GhostPad>().ok())
                .ok_or_else(|| format!("no {name} ghost pad"))?
                .set_target(Some(&target))
                .map_err(|err| err.to_string())?;
        }
        let input = shader.static_pad("sink").ok_or("no shader sink pad")?;
        // The shader works in source pixels; their count arrives with the
        // caps, before any frame drawn in it.
        let weak = obj.downgrade();
        input.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && let gst::EventView::Caps(caps) = event.view()
                && let Ok(video) = gst_video::VideoInfo::from_caps(caps.caps())
                && let Some(obj) = weak.upgrade()
            {
                obj.imp()
                    .push_uniforms(|drawing| drawing.source = (video.width(), video.height()));
            }
            gst::PadProbeReturn::Ok
        });
        // A new output size renegotiates the shader's output only: its input
        // stays an RGBA texture, so the reconfigure that the size capsfilter
        // sends upstream stops at the shader instead of making the whole
        // pipeline, through any tee, query caps again.
        input.add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            |_pad, info| match &info.data {
                Some(gst::PadProbeData::Event(event))
                    if event.type_() == gst::EventType::Reconfigure =>
                {
                    gst::PadProbeReturn::Drop
                }
                _ => gst::PadProbeReturn::Ok,
            },
        );
        Ok(Children { shader, size })
    }

    /// Apply `change` and hand the shader the current settings, which it
    /// takes before its next frame. The shader keeps only the last uniforms
    /// it was given, so they are read and handed over under one lock: an
    /// update read before another and handed over after it would undo it.
    fn push_uniforms(&self, change: impl FnOnce(&mut Drawing)) {
        let mut drawing = lock(&self.drawing);
        change(&mut drawing);
        let Ok(children) = self.children() else {
            return;
        };
        let uniforms = {
            let settings = lock(&self.settings);
            uniforms(
                &drawing.quad,
                drawing.source,
                settings.interpolation,
                settings.background,
            )
        };
        children.shader.set_property("uniforms", uniforms);
    }

    /// Give the shader its output size; the next frame renegotiates. Under
    /// the settings lock, so the last size set is the last handed over.
    fn update_size(&self) {
        let Ok(children) = self.children() else {
            return;
        };
        let settings = lock(&self.settings);
        let caps = size_caps(settings.output_width, settings.output_height);
        children.size.set_property("caps", caps);
    }

    /// Take a new quad. While streaming, an invalid one is ignored.
    fn set_quad(&self, text: String) {
        let quad = parse_quad(&text);
        let streaming = self.streaming.load(Ordering::SeqCst);
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
        if let Ok(quad) = quad {
            self.push_uniforms(|drawing| drawing.quad = quad);
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for GlWallWarp {
    const NAME: &'static str = "GstSmithGlWallWarp";
    type Type = super::GlWallWarp;
    type ParentType = gst::Bin;
}

impl ObjectImpl for GlWallWarp {
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
                    .blurb("Output width in pixels; 0 negotiates it")
                    .maximum(MAX_OUTPUT_SIZE)
                    .default_value(DEFAULT_OUTPUT_SIZE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("output-height")
                    .nick("Output height")
                    .blurb("Output height in pixels; 0 negotiates it")
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
            "output-width" => {
                lock(&self.settings).output_width = value.get().unwrap_or(DEFAULT_OUTPUT_SIZE);
                self.update_size();
            }
            "output-height" => {
                lock(&self.settings).output_height = value.get().unwrap_or(DEFAULT_OUTPUT_SIZE);
                self.update_size();
            }
            "interpolation" => {
                lock(&self.settings).interpolation = value.get().unwrap_or_default();
                self.push_uniforms(|_drawing| ());
            }
            "background" => {
                lock(&self.settings).background = value.get().unwrap_or(DEFAULT_BACKGROUND);
                self.push_uniforms(|_drawing| ());
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
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        for template in obj.pad_template_list() {
            let pad = gst::GhostPad::builder_from_template(&template).build();
            if let Err(err) = obj.add_pad(&pad) {
                gst::error!(
                    CAT,
                    imp = self,
                    "adding pad {}: {err}",
                    template.name_template()
                );
            }
        }
        let children = self.build();
        if let Err(err) = &children {
            gst::warning!(CAT, imp = self, "{err}");
        }
        if self.children.set(children).is_err() {
            gst::error!(CAT, imp = self, "constructed twice");
        }
        self.push_uniforms(|_drawing| ());
        self.update_size();
    }
}

impl GstObjectImpl for GlWallWarp {}

impl ElementImpl for GlWallWarp {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Video wall warp (OpenGL)",
                "Filter/Converter/Video/Scaler/Hardware",
                "Maps a quadrilateral region of the input onto the whole output frame through a \
                 perspective transform on the GPU, producing the stream for one video wall display",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // Any video glupload takes, out as any glcolorconvert and
            // gldownload give.
            let caps =
                gst::Caps::from_str("video/x-raw(ANY)").unwrap_or_else(|_err| gst::Caps::new_any());
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

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                if let Err(err) = self.children() {
                    gst::element_imp_error!(self, gst::CoreError::MissingPlugin, ["{err}"]);
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToPaused => {
                {
                    let settings = lock(&self.settings);
                    if let Err(err) = &settings.quad {
                        gst::element_imp_error!(
                            self,
                            gst::LibraryError::Settings,
                            ("invalid quad {:?}: {err}", settings.quad_text)
                        );
                        return Err(gst::StateChangeError);
                    }
                }
                self.streaming.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
        let result = self.parent_change_state(transition);
        if transition == gst::StateChange::PausedToReady || result.is_err() {
            self.streaming.store(false, Ordering::SeqCst);
        }
        result
    }
}

impl BinImpl for GlWallWarp {}
