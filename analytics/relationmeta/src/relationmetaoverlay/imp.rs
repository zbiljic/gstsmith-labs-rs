use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_analytics::AnalyticsRelationMeta;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use super::render::{self, Palette, Style};
use super::scene;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "relationmetaoverlay",
        gst::DebugColorFlags::empty(),
        Some("Draws analytics relation metadata"),
    )
});

const DEFAULT_EXPIRE: u64 = 1_000_000_000;

/// Formats `gst_video_blend` handles, as accepted by upstream text overlays.
const FORMATS: &[gst_video::VideoFormat] = {
    use gst_video::VideoFormat as F;
    &[
        F::Bgrx,
        F::Rgbx,
        F::Xrgb,
        F::Xbgr,
        F::Rgba,
        F::Bgra,
        F::Argb,
        F::Abgr,
        F::Rgb,
        F::Bgr,
        F::I420,
        F::Yv12,
        F::Ayuv,
        F::Yuy2,
        F::Uyvy,
        F::V308,
        F::Y41b,
        F::Y42b,
        F::Y444,
        F::Nv12,
        F::Nv21,
        F::A420,
        F::Yuv9,
        F::Yvu9,
        F::Iyu1,
        F::Gray8,
    ]
};

#[derive(Clone, Debug)]
struct Settings {
    style: Style,
    expire: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            style: Style::default(),
            expire: DEFAULT_EXPIRE,
        }
    }
}

#[derive(Default)]
struct State {
    info: Option<gst_video::VideoInfo>,
    /// Downstream renders `VideoOverlayCompositionMeta` itself; decided on
    /// the first buffer after caps, once downstream has them too.
    attach: Option<bool>,
    /// The last result and the running time it arrived at.
    last: Option<Last>,
}

struct Last {
    composition: Option<gst_video::VideoOverlayComposition>,
    since: Option<u64>,
    /// What it was rendered from, so a re-attached result is not redrawn.
    scene: scene::Scene,
    style: Style,
}

#[derive(Default)]
pub struct RelationMetaOverlay {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RelationMetaOverlay {
    fn running_time(&self, buffer: &gst::BufferRef) -> Option<u64> {
        self.obj()
            .segment()
            .downcast_ref::<gst::ClockTime>()
            .and_then(|segment| segment.to_running_time(buffer.pts()))
            .map(gst::ClockTime::nseconds)
    }

    /// Asks downstream whether it renders overlay compositions. An in-place
    /// `BaseTransform` never runs `decide_allocation`, and `set_caps` runs
    /// before downstream has caps, so this runs on the first buffer after.
    fn downstream_renders_overlays(&self) -> bool {
        let element = self.obj();
        let src = element.src_pad();
        let Some(caps) = src.current_caps() else {
            return false;
        };
        let mut query = gst::query::Allocation::new(Some(&caps), false);
        let attach = src.peer_query(&mut query)
            && query
                .find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                .is_some();
        gst::debug!(CAT, imp = self, "downstream renders overlay meta: {attach}");
        attach
    }

    /// The composition for this buffer: a new result when it carries relation
    /// meta, otherwise the last result while it has not expired.
    fn composition(
        &self,
        buffer: &gst::BufferRef,
        info: &gst_video::VideoInfo,
        settings: &Settings,
    ) -> Result<Option<gst_video::VideoOverlayComposition>, gst::FlowError> {
        let now = self.running_time(buffer);
        let mut state = lock(&self.state);
        if let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() {
            let scene = scene::read(&meta, info.width(), info.height());
            if let Some(last) = &mut state.last
                && last.scene == scene
                && last.style == settings.style
            {
                last.since = now;
                return Ok(last.composition.clone());
            }
            let composition = render::render(&scene, info.width(), info.height(), &settings.style)
                .map_err(|err| {
                    gst::element_imp_error!(self, gst::StreamError::Failed, ["{err}"]);
                    gst::FlowError::Error
                })?;
            state.last = Some(Last {
                composition: composition.clone(),
                since: now,
                scene,
                style: settings.style.clone(),
            });
            return Ok(composition);
        }
        let Some(last) = &state.last else {
            return Ok(None);
        };
        let fresh = match (last.since, now) {
            (Some(since), Some(now)) => now.saturating_sub(since) <= settings.expire,
            _ => settings.expire > 0,
        };
        if fresh {
            Ok(last.composition.clone())
        } else {
            state.last = None;
            Ok(None)
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RelationMetaOverlay {
    const NAME: &'static str = "GstSmithRelationMetaOverlay";
    type Type = super::RelationMetaOverlay;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for RelationMetaOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let defaults = Style::default();
            let flag = |name: &'static str, blurb: &'static str, default: bool| {
                glib::ParamSpecBoolean::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .default_value(default)
                    .mutable_playing()
                    .build()
            };
            vec![
                flag("draw-masks", "Draw polygons and segmentation masks", defaults.draw_masks),
                flag("draw-boxes", "Draw object boxes", defaults.draw_boxes),
                flag("draw-relations", "Draw relation arrows with predicates", defaults.draw_relations),
                flag("draw-labels", "Draw label chips with scores", defaults.draw_labels),
                flag("draw-text", "Draw recognized text instead of text-line labels", defaults.draw_text),
                flag("draw-caption", "Draw the frame caption", defaults.draw_caption),
                glib::ParamSpecString::builder("font-desc")
                    .nick("Font description")
                    .blurb("Pango font description of all chips, in pixels")
                    .default_value(Some(defaults.font.as_str()))
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("line-width")
                    .nick("Line width")
                    .blurb("Width of boxes, outlines, arrows and chip borders in pixels")
                    .minimum(0.5)
                    .maximum(20.0)
                    .default_value(defaults.line_width)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("polygon-opacity")
                    .nick("Polygon opacity")
                    .blurb("Opacity of polygon fills; outlines are at least 0.35")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.polygon_opacity)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("mask-opacity")
                    .nick("Mask opacity")
                    .blurb("Opacity of segmentation mask fills")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.mask_opacity)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("palette", defaults.palette)
                    .nick("Palette")
                    .blurb("How objects are coloured")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("expire-overlay")
                    .nick("Expire overlay")
                    .blurb("How long in ns the last result stays drawn on frames without relation meta; 0 draws only frames that carry it")
                    .default_value(DEFAULT_EXPIRE)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        let style = &mut settings.style;
        match pspec.name() {
            "draw-masks" => style.draw_masks = value.get().unwrap_or(style.draw_masks),
            "draw-boxes" => style.draw_boxes = value.get().unwrap_or(style.draw_boxes),
            "draw-relations" => style.draw_relations = value.get().unwrap_or(style.draw_relations),
            "draw-labels" => style.draw_labels = value.get().unwrap_or(style.draw_labels),
            "draw-text" => style.draw_text = value.get().unwrap_or(style.draw_text),
            "draw-caption" => style.draw_caption = value.get().unwrap_or(style.draw_caption),
            "font-desc" => {
                style.font = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| Style::default().font);
            }
            "line-width" => style.line_width = value.get().unwrap_or(style.line_width),
            "polygon-opacity" => {
                style.polygon_opacity = value.get().unwrap_or(style.polygon_opacity);
            }
            "mask-opacity" => style.mask_opacity = value.get().unwrap_or(style.mask_opacity),
            "palette" => style.palette = value.get().unwrap_or(Palette::Pastel),
            "expire-overlay" => settings.expire = value.get().unwrap_or(DEFAULT_EXPIRE),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        let style = &settings.style;
        match pspec.name() {
            "draw-masks" => style.draw_masks.to_value(),
            "draw-boxes" => style.draw_boxes.to_value(),
            "draw-relations" => style.draw_relations.to_value(),
            "draw-labels" => style.draw_labels.to_value(),
            "draw-text" => style.draw_text.to_value(),
            "draw-caption" => style.draw_caption.to_value(),
            "font-desc" => style.font.to_value(),
            "line-width" => style.line_width.to_value(),
            "polygon-opacity" => style.polygon_opacity.to_value(),
            "mask-opacity" => style.mask_opacity.to_value(),
            "palette" => style.palette.to_value(),
            "expire-overlay" => settings.expire.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for RelationMetaOverlay {}

impl ElementImpl for RelationMetaOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Relation meta overlay",
                "Filter/Editor/Video",
                "Draws boxes, outlines, relations, labels, text and captions from analytics relation metadata",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list(FORMATS.iter().copied())
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
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for RelationMetaOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *lock(&self.state) = State::default();
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|err| gst::loggable_error!(CAT, "invalid caps {incaps}: {err}"))?;
        let mut state = lock(&self.state);
        state.info = Some(info);
        state.attach = None;
        state.last = None;
        drop(state);
        self.parent_set_caps(incaps, outcaps)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::FlushStop(_) | gst::EventView::StreamStart(_)
        ) {
            lock(&self.state).last = None;
        }
        self.parent_sink_event(event)
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = lock(&self.settings).clone();
        let (info, attach) = {
            let state = lock(&self.state);
            (
                state.info.clone().ok_or(gst::FlowError::NotNegotiated)?,
                state.attach,
            )
        };
        let attach = attach.unwrap_or_else(|| {
            let attach = self.downstream_renders_overlays();
            lock(&self.state).attach = Some(attach);
            attach
        });
        let Some(composition) = self.composition(buffer, &info, &settings)? else {
            return Ok(gst::FlowSuccess::Ok);
        };
        if attach {
            // Merge with an upstream overlay instead of adding a second meta.
            let merged = match buffer.meta_mut::<gst_video::VideoOverlayCompositionMeta>() {
                Some(mut existing) => {
                    let rectangles: Vec<_> = existing
                        .overlay()
                        .iter()
                        .chain(composition.iter())
                        .collect();
                    let merged = gst_video::VideoOverlayComposition::new(rectangles.iter())
                        .map_err(|_err| gst::FlowError::Error)?;
                    existing.set_overlay(&merged);
                    true
                }
                None => false,
            };
            if !merged {
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            }
        } else {
            let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info)
                .map_err(|err| {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Failed,
                        ["mapping the frame: {err}"]
                    );
                    gst::FlowError::Error
                })?;
            composition.blend(&mut frame).map_err(|err| {
                gst::element_imp_error!(self, gst::CoreError::Failed, ["blending: {err}"]);
                gst::FlowError::Error
            })?;
        }
        Ok(gst::FlowSuccess::Ok)
    }
}
