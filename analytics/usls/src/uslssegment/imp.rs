use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

use anyhow::{Context, Result, ensure};
use gst::{glib, prelude::*};
use gst_analytics::prelude::*;
use gst_base::subclass::prelude::*;

use crate::{
    backend::{ComputeUnits, Provider},
    frame,
    segment::{Backend, Options, Profile, Prompt, Region, parse_prompt},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "uslssegment",
        gst::DebugColorFlags::empty(),
        Some("usls segmentation"),
    )
});

#[derive(Clone)]
struct Settings {
    model: Option<PathBuf>,
    encoder: Option<PathBuf>,
    decoder: Option<PathBuf>,
    prompt: Option<String>,
    mask_threshold: f32,
    mask_maximum: u32,
    profile: Profile,
    threshold: f32,
    cap: u32,
    maximum: u32,
    provider: Provider,
    units: ComputeUnits,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            model: None,
            encoder: None,
            decoder: None,
            prompt: None,
            mask_threshold: 0.5,
            mask_maximum: 64 * 1024 * 1024,
            profile: Profile::FastSam,
            threshold: 0.5,
            cap: 16,
            maximum: 32 * 1024 * 1024,
            provider: Provider::Cpu,
            units: ComputeUnits::CpuAndGpu,
        }
    }
}

struct State {
    backend: Backend,
    info: Option<gst_video::VideoInfo>,
    maximum: u32,
    options: Options,
    prompt: Option<Prompt>,
}

#[derive(Default)]
pub struct UslsSegment {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    #[cfg(test)]
    fixture: Mutex<Option<Backend>>,
}

#[glib::object_subclass]
impl ObjectSubclass for UslsSegment {
    const NAME: &'static str = "GstUslsSegment";
    type Type = super::UslsSegment;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for UslsSegment {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder::<Profile>("model-profile")
                    .blurb("Segmentation family and preprocessing; local graphs must match the profile")
                    .default_value(Profile::FastSam)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("model-file")
                    .blurb("Required local compatible FP32 batch1 ONNX graph for model-profile")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("encoder-file").blurb("Required local image encoder for prompted profiles").mutable_ready().build(),
                glib::ParamSpecString::builder("decoder-file").blurb("Required local mask decoder for prompted profiles").mutable_ready().build(),
                glib::ParamSpecString::builder("prompt").blurb("Fixed source-pixel prompt: box:x,y,w,h;point:x,y;exclude:x,y. Unset uses this frame's detection metadata.").mutable_ready().build(),
                glib::ParamSpecFloat::builder("mask-threshold").minimum(0.0).maximum(1.0).default_value(0.5).mutable_ready().build(),
                glib::ParamSpecUInt::builder("max-mask-bytes").minimum(1).maximum(512*1024*1024).default_value(64*1024*1024).mutable_ready().build(),
                glib::ParamSpecFloat::builder("score-threshold")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(0.5)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-regions")
                    .minimum(1)
                    .maximum(1000)
                    .default_value(16)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-frame-bytes")
                    .minimum(1)
                    .maximum(128 * 1024 * 1024)
                    .default_value(32 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<Provider>("execution-provider")
                    .default_value(Provider::Cpu)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<ComputeUnits>("coreml-compute-units")
                    .default_value(ComputeUnits::CpuAndGpu)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    #[expect(
        clippy::expect_used,
        reason = "GObject validates property value types before dispatch"
    )]
    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if self.obj().current_state() > gst::State::Ready {
            gst::warning!(
                CAT,
                imp = self,
                "{} is only mutable through READY",
                pspec.name()
            );
            return;
        }
        let Ok(mut settings) = self.settings.lock() else {
            return;
        };
        match pspec.name() {
            "model-file" => {
                settings.model = value
                    .get::<Option<String>>()
                    .expect("string property")
                    .map(PathBuf::from);
            }
            "encoder-file" => {
                settings.encoder = value
                    .get::<Option<String>>()
                    .expect("encoder path")
                    .map(PathBuf::from);
            }
            "decoder-file" => {
                settings.decoder = value
                    .get::<Option<String>>()
                    .expect("decoder path")
                    .map(PathBuf::from);
            }
            "prompt" => settings.prompt = value.get().expect("prompt string"),
            "mask-threshold" => settings.mask_threshold = value.get().expect("mask threshold"),
            "max-mask-bytes" => settings.mask_maximum = value.get().expect("mask byte limit"),
            "model-profile" => settings.profile = value.get().expect("profile property"),
            "score-threshold" => settings.threshold = value.get().expect("float property"),
            "max-regions" => settings.cap = value.get().expect("uint property"),
            "max-frame-bytes" => settings.maximum = value.get().expect("uint property"),
            "execution-provider" => settings.provider = value.get().expect("provider property"),
            "coreml-compute-units" => settings.units = value.get().expect("compute-units property"),
            _ => {}
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self
            .settings
            .lock()
            .map_or_else(|_| Settings::default(), |s| s.clone());
        match pspec.name() {
            "model-file" => settings
                .model
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .to_value(),
            "encoder-file" => settings
                .encoder
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .to_value(),
            "decoder-file" => settings
                .decoder
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .to_value(),
            "prompt" => settings.prompt.to_value(),
            "mask-threshold" => settings.mask_threshold.to_value(),
            "max-mask-bytes" => settings.mask_maximum.to_value(),
            "model-profile" => settings.profile.to_value(),
            "score-threshold" => settings.threshold.to_value(),
            "max-regions" => settings.cap.to_value(),
            "max-frame-bytes" => settings.maximum.to_value(),
            "execution-provider" => settings.provider.to_value(),
            "coreml-compute-units" => settings.units.to_value(),
            _ => glib::Value::from_type(pspec.value_type()),
        }
    }
}
impl GstObjectImpl for UslsSegment {}
impl ElementImpl for UslsSegment {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "usls Segmentation",
                "Filter/Analysis/Video",
                "Attaches synchronous source-frame segmentation analytics",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&METADATA)
    }
    #[expect(
        clippy::expect_used,
        reason = "fixed valid RGB caps and static pad names"
    )]
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .field("width", gst::IntRange::<i32>::new(1, i32::MAX))
                .field("height", gst::IntRange::<i32>::new(1, i32::MAX))
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("sink template"),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("src template"),
            ]
        });
        TEMPLATES.as_ref()
    }
}
impl BaseTransformImpl for UslsSegment {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> std::result::Result<(), gst::ErrorMessage> {
        #[cfg(test)]
        if let Some(backend) = self
            .fixture
            .lock()
            .map_err(|_error| gst::error_msg!(gst::LibraryError::Init, ["fixture lock poisoned"]))?
            .take()
        {
            *self.state.lock().map_err(|_error| {
                gst::error_msg!(gst::LibraryError::Init, ["state lock poisoned"])
            })? = Some(State {
                backend,
                info: None,
                maximum: 32 * 1024 * 1024,
                options: Options {
                    profile: Profile::Sam,
                    ..Options::default()
                },
                prompt: None,
            });
            return Ok(());
        }
        let initialize = || -> Result<State> {
            let settings = self
                .settings
                .lock()
                .map_err(|_error| anyhow::anyhow!("settings lock poisoned"))?
                .clone();
            let prompt = settings.prompt.as_deref().map(parse_prompt).transpose()?;
            ensure!(
                settings.profile.prompted() || prompt.is_none(),
                "fixed prompts require a prompted profile"
            );
            let options = Options {
                profile: settings.profile,
                threshold: settings.threshold,
                mask_threshold: settings.mask_threshold,
                cap: usize::try_from(settings.cap)?,
                maximum: usize::try_from(settings.mask_maximum)?,
                provider: settings.provider,
                units: settings.units,
            };
            ensure!(
                (1..=128 * 1024 * 1024).contains(&settings.maximum),
                "invalid max-frame-bytes"
            );
            Ok(State {
                backend: Backend::new(
                    settings.model.as_deref(),
                    settings.encoder.as_deref(),
                    settings.decoder.as_deref(),
                    options.clone(),
                )?,
                options,
                prompt,
                info: None,
                maximum: settings.maximum,
            })
        };
        let state = catch_unwind(AssertUnwindSafe(initialize))
            .map_err(|_error| {
                gst::error_msg!(
                    gst::LibraryError::Init,
                    ["upstream model initialization panicked"]
                )
            })?
            .map_err(|error| {
                gst::error!(CAT, imp = self, "{error:#}");
                gst::error_msg!(gst::LibraryError::Init, ["{error:#}"])
            })?;
        *self.state.lock().map_err(|_error| {
            gst::error_msg!(gst::LibraryError::Init, ["state lock poisoned"])
        })? = Some(state);
        Ok(())
    }
    fn stop(&self) -> std::result::Result<(), gst::ErrorMessage> {
        let state = self
            .state
            .lock()
            .map_err(|_error| {
                gst::error_msg!(gst::LibraryError::Shutdown, ["state lock poisoned"])
            })?
            .take();
        catch_unwind(AssertUnwindSafe(|| drop(state))).map_err(|_error| {
            gst::error_msg!(
                gst::LibraryError::Shutdown,
                ["upstream model shutdown panicked"]
            )
        })
    }
    fn set_caps(
        &self,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> std::result::Result<(), gst::LoggableError> {
        let negotiate = || -> Result<()> {
            ensure!(incaps == outcaps, "input and output caps must be identical");
            let info = gst_video::VideoInfo::from_caps(incaps)?;
            ensure!(
                info.format() == gst_video::VideoFormat::Rgb
                    && info.par() == gst::Fraction::new(1, 1),
                "only square-pixel RGB is supported"
            );
            let mut state = self
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock poisoned"))?;
            let state = state.as_mut().context("element is not started")?;
            ensure!(
                info.width() <= 65535 && info.height() <= 65535,
                "segmentation frame dimensions exceed 65535"
            );
            frame::packed_size(info.width(), info.height(), state.maximum)?;
            state.info = Some(info);
            Ok(())
        };
        negotiate().map_err(|error| gst::loggable_error!(CAT, "{error:#}"))?;
        self.parent_set_caps(incaps, outcaps)
    }
    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> std::result::Result<gst::FlowSuccess, gst::FlowError> {
        let result = self.process(buffer);
        match result {
            Ok(()) => Ok(gst::FlowSuccess::Ok),
            Err(error) => {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ("usls inference failed"),
                    ["{error:#}"]
                );
                Err(gst::FlowError::Error)
            }
        }
    }
}
impl UslsSegment {
    fn process(&self, buffer: &mut gst::BufferRef) -> Result<()> {
        let (regions, width, height, semantic) = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock poisoned"))?;
            let state = guard.as_mut().context("runtime is unavailable")?;
            let info = state.info.as_ref().context("RGB caps are not negotiated")?;
            let (width, height) = (info.width(), info.height());
            let semantic = state.options.profile.semantic();
            let prompts = if state.options.profile.prompted() {
                if let Some(prompt) = &state.prompt {
                    vec![prompt.clone()]
                } else {
                    detection_prompts(
                        buffer,
                        width,
                        height,
                        state.options.threshold,
                        state.options.cap,
                    )?
                }
            } else {
                vec![]
            };
            if state.options.profile.prompted() && prompts.is_empty() {
                frame::validate_rgb(buffer, info, state.maximum)?;
                let pixels = frame::packed_size(width, height, state.maximum)? / 3;
                ensure!(
                    pixels <= state.options.maximum,
                    "one mask exceeds max-mask-bytes"
                );
                return Ok(());
            }
            let result = catch_unwind(AssertUnwindSafe(|| -> Result<Vec<Region>> {
                let image = frame::image_rgb(buffer, info, state.maximum)?;
                state.backend.run(&image, &prompts)
            }));
            let regions = if let Ok(result) = result {
                result?
            } else {
                let poisoned = guard.take();
                let dropped = catch_unwind(AssertUnwindSafe(|| drop(poisoned)));
                ensure!(dropped.is_ok(), "upstream runtime panicked during discard");
                anyhow::bail!("upstream inference panicked; runtime discarded");
            };
            (regions, width, height, semantic)
        };
        attach_metadata(buffer, regions, width, height, semantic)
    }
}

fn detection_prompts(
    buffer: &gst::BufferRef,
    width: u32,
    height: u32,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Prompt>> {
    let Some(meta) = buffer.meta::<gst_analytics::AnalyticsRelationMeta>() else {
        return Ok(vec![]);
    };
    let mut prompts = Vec::new();
    for object in meta.iter::<gst_analytics::AnalyticsODMtd>() {
        let location = object.location()?;
        ensure!(
            location.loc_conf_lvl.is_finite(),
            "nonfinite detection prompt confidence"
        );
        if location.loc_conf_lvl < threshold {
            continue;
        }
        let x1 = i64::from(location.x).clamp(0, i64::from(width));
        let y1 = i64::from(location.y).clamp(0, i64::from(height));
        let x2 = (i64::from(location.x) + i64::from(location.w)).clamp(0, i64::from(width));
        let y2 = (i64::from(location.y) + i64::from(location.h)).clamp(0, i64::from(height));
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        prompts.push((
            location.loc_conf_lvl,
            Prompt {
                bbox: Some([
                    f32::from(u16::try_from(x1)?),
                    f32::from(u16::try_from(y1)?),
                    f32::from(u16::try_from(x2)?),
                    f32::from(u16::try_from(y2)?),
                ]),
                points: vec![],
                source: Some(object.id()),
                label: object
                    .obj_type()
                    .unwrap_or_else(|| glib::Quark::from_str("object")),
            },
        ));
        // Keep prompt storage bounded even when upstream metadata is very large.
        prompts.sort_by(|a, b| b.0.total_cmp(&a.0));
        prompts.truncate(cap);
    }
    Ok(prompts.into_iter().map(|(_, prompt)| prompt).collect())
}

fn attach_metadata(
    buffer: &mut gst::BufferRef,
    regions: Vec<Region>,
    width: u32,
    height: u32,
    semantic: bool,
) -> Result<()> {
    if regions.is_empty() {
        return Ok(());
    }
    let exists = buffer
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .is_some();
    let mut relation = if exists {
        buffer
            .meta_mut::<gst_analytics::AnalyticsRelationMeta>()
            .context("existing relation metadata is not writable")?
    } else {
        gst_analytics::AnalyticsRelationMeta::add(buffer)
    };
    for region in regions {
        ensure!(region.score.is_finite(), "invalid segmentation confidence");
        let mask = segmentation_meta::add(&mut relation, region.pixels, width, height, semantic)?;
        let class = relation
            .add_cls_mtd(
                &[-1.0, region.score],
                &[glib::Quark::from_str("background"), region.label],
            )?
            .id();
        relation.set_relation(gst_analytics::RelTypes::RELATE_TO, mask, class)?;
        let object = if let Some(id) = region.source {
            Some(id)
        } else if let Some([x1, y1, x2, y2]) = region.bbox {
            ensure!(
                [x1, y1, x2, y2].iter().all(|v| v.is_finite())
                    && x1 >= 0.0
                    && y1 >= 0.0
                    && x2 <= f64::from(width)
                    && y2 <= f64::from(height),
                "invalid region box"
            );
            #[expect(
                clippy::cast_possible_truncation,
                reason = "finite source coordinates are clamped to the positive negotiated i32 frame bounds"
            )]
            let (x, y, r, b) = (
                x1.floor() as i32,
                y1.floor() as i32,
                x2.ceil() as i32,
                y2.ceil() as i32,
            );
            Some(
                relation
                    .add_od_mtd(region.label, x, y, r - x, b - y, region.score)?
                    .id(),
            )
        } else {
            None
        };
        if let Some(object) = object {
            relation.set_relation(gst_analytics::RelTypes::RELATE_TO, object, mask)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::subclass::prelude::ObjectSubclassIsExt;

    fn fake(image: &usls::Image, prompts: &[Prompt]) -> Result<Vec<Region>> {
        ensure!(
            image.width() == 3
                && image.height() == 2
                && image.as_raw() == &(1..=18).collect::<Vec<u8>>(),
            "stride and source pixels"
        );
        prompts
            .iter()
            .map(|prompt| {
                ensure!(
                    prompt.bbox == Some([0.0, 0.0, 2.0, 2.0]),
                    "clipped source box"
                );
                Ok(Region {
                    pixels: vec![1, 0, 0, 1, 0, 0],
                    label: prompt.label,
                    score: 0.8,
                    source: prompt.source,
                    bbox: None,
                })
            })
            .collect()
    }
    #[expect(
        clippy::panic_in_result_fn,
        reason = "fixture checks unwind containment"
    )]
    fn panics(_image: &usls::Image, _prompts: &[Prompt]) -> Result<Vec<Region>> {
        panic!("fixture panic")
    }
    fn element(
        infer: fn(&usls::Image, &[Prompt]) -> Result<Vec<Region>>,
    ) -> Result<super::super::UslsSegment> {
        gst::init()?;
        let e: super::super::UslsSegment = glib::Object::new();
        *e.imp()
            .fixture
            .lock()
            .map_err(|_error| anyhow::anyhow!("fixture lock"))? = Some(Backend::Fixture(infer));
        Ok(e)
    }
    fn buffer(label: Option<&str>) -> Result<gst::Buffer> {
        let mut bytes = vec![99; 5];
        bytes.extend(1..=9);
        bytes.extend([99; 7]);
        bytes.extend(10..=18);
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let b = buffer.make_mut();
        b.set_pts(gst::ClockTime::from_mseconds(123));
        b.set_duration(gst::ClockTime::from_mseconds(40));
        b.set_offset(7);
        b.set_flags(gst::BufferFlags::DISCONT);
        gst_video::VideoMeta::add_full(
            b,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgb,
            3,
            2,
            &[5],
            &[16],
        )?;
        let mut meta = gst_analytics::AnalyticsRelationMeta::add(b);
        meta.add_one_cls_mtd(0.4, glib::Quark::from_str("prior"))?;
        if let Some(label) = label {
            meta.add_od_mtd(glib::Quark::from_str(label), -1, 0, 3, 2, 0.9)?;
        }
        Ok(buffer)
    }
    fn harness(e: &super::super::UslsSegment) -> gst_check::Harness {
        let mut h = gst_check::Harness::with_element(e, Some("sink"), Some("src"));
        h.set_src_caps_str(
            "video/x-raw,format=RGB,width=3,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
        );
        h.play();
        h
    }
    #[test]
    fn empty_prompts_skip_backend_but_preserve_validation() -> Result<()> {
        let e = element(panics)?;
        let mut h = harness(&e);
        let input = buffer(None)?;
        let expected = input.map_readable()?.as_slice().to_vec();
        let output = h.push_and_pull(input)?;
        ensure!(
            output.map_readable()?.as_slice() == expected,
            "carrier changed"
        );
        ensure!(
            output.pts() == Some(gst::ClockTime::from_mseconds(123)),
            "PTS changed"
        );
        ensure!(
            e.imp()
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock"))?
                .is_some(),
            "backend called"
        );
        let mut malformed = gst::Buffer::with_size(1)?;
        e.imp()
            .process(malformed.make_mut())
            .expect_err("short carrier");
        {
            let mut guard = e
                .imp()
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock"))?;
            guard.as_mut().context("state")?.options.maximum = 5;
        }
        e.imp()
            .process(buffer(None)?.make_mut())
            .expect_err("mask budget");
        Ok(())
    }
    #[test]
    fn masks_follow_this_frames_detections_and_preserve_carrier() -> Result<()> {
        let e = element(fake)?;
        let mut h = harness(&e);
        for label in [Some("person"), Some("dog"), None] {
            let input = buffer(label)?;
            let expected = input.map_readable()?.as_slice().to_vec();
            let output = h.push_and_pull(input)?;
            ensure!(
                output.map_readable()?.as_slice() == expected
                    && output.pts() == Some(gst::ClockTime::from_mseconds(123))
                    && output.duration() == Some(gst::ClockTime::from_mseconds(40))
                    && output.offset() == 7
                    && output.flags().contains(gst::BufferFlags::DISCONT),
                "carrier preservation"
            );
            ensure!(
                output
                    .iter_meta::<gst_analytics::AnalyticsRelationMeta>()
                    .count()
                    == 1,
                "reuse relation meta"
            );
            let meta = output
                .meta::<gst_analytics::AnalyticsRelationMeta>()
                .context("meta")?;
            let masks = meta
                .iter::<gst_analytics::AnalyticsAnyMtd>()
                .filter_map(|m| segmentation_meta::mask(&meta, m.id()).map(|b| (m.id(), b)))
                .collect::<Vec<_>>();
            ensure!(
                masks.len() == usize::from(label.is_some()),
                "no stale prompts"
            );
            if let Some(label) = label {
                let (id, mask) = masks.first().context("mask")?;
                ensure!(
                    mask.map_readable()?.as_slice() == [1, 0, 0, 1, 0, 0],
                    "binary mask"
                );
                let object = meta
                    .iter::<gst_analytics::AnalyticsODMtd>()
                    .next()
                    .context("original object")?;
                ensure!(
                    meta.relation(object.id(), *id) == gst_analytics::RelTypes::RELATE_TO
                        && object.obj_type() == Some(glib::Quark::from_str(label)),
                    "mask linked to original object"
                );
                ensure!(
                    meta.iter::<gst_analytics::AnalyticsClassificationMtd>()
                        .any(
                            |c| meta.relation(*id, c.id()) == gst_analytics::RelTypes::RELATE_TO
                                && c.iterate()
                                    .any(|(name, _)| name == glib::Quark::from_str(label))
                        ),
                    "mask classification"
                );
            }
        }
        ensure!(h.push_event(gst::event::FlushStart::new()), "flush start");
        ensure!(h.push_event(gst::event::FlushStop::new(true)), "flush stop");
        ensure!(
            h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new())),
            "segment"
        );
        h.push_and_pull(buffer(None)?)?;
        ensure!(h.push_event(gst::event::Eos::new()), "EOS");
        Ok(())
    }
    #[test]
    fn panic_discards_runtime_and_settings_freeze_while_streaming() -> Result<()> {
        let e = element(panics)?;
        let mut h = harness(&e);
        e.set_property_from_str("model-profile", "birefnet-dis");
        ensure!(
            e.property::<Profile>("model-profile") == Profile::FastSam,
            "READY-only property"
        );
        ensure!(
            h.push(buffer(Some("person"))?) == Err(gst::FlowError::Error),
            "panic becomes flow error"
        );
        ensure!(
            e.imp()
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock"))?
                .is_none(),
            "discard poisoned runtime"
        );
        Ok(())
    }
    #[test]
    fn masks_preserve_overlaps_and_support_standard_overlay() -> Result<()> {
        gst::init()?;
        let mut b = gst::Buffer::with_size(24)?;
        let regions = (0..2)
            .map(|_| Region {
                pixels: vec![1; 6],
                label: glib::Quark::from_str("object"),
                score: 0.8,
                source: None,
                bbox: None,
            })
            .collect();
        attach_metadata(b.make_mut(), regions, 3, 2, false)?;
        let mut h = gst_check::Harness::new("segmentationoverlay");
        h.set_src_caps_str(
            "video/x-raw,format=RGB,width=3,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
        );
        h.play();
        let output = h.push_and_pull(b)?;
        ensure!(
            output.size() == 24,
            "overlay accepted two overlapping masks"
        );
        Ok(())
    }
}
