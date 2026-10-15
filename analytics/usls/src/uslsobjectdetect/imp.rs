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
    backend::{Backend, ComputeUnits, Detection, Profile, Provider},
    frame,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "uslsobjectdetect",
        gst::DebugColorFlags::empty(),
        Some("usls object detection"),
    )
});

#[derive(Clone)]
struct Settings {
    model: Option<PathBuf>,
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
            profile: Profile::RfdetrNano,
            threshold: 0.5,
            cap: 100,
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
}

#[derive(Default)]
pub struct UslsObjectDetect {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    #[cfg(test)]
    fixture: Mutex<Option<Backend>>,
}

#[glib::object_subclass]
impl ObjectSubclass for UslsObjectDetect {
    const NAME: &'static str = "GstUslsObjectDetect";
    type Type = super::UslsObjectDetect;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for UslsObjectDetect {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder::<Profile>("model-profile")
                    .blurb("COCO detector family and preprocessing; model-file must match this profile")
                    .default_value(Profile::RfdetrNano)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("model-file")
                    .blurb("Required local compatible FP32 batch1 ONNX graph for model-profile")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("score-threshold")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(0.5)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-detections")
                    .minimum(1)
                    .maximum(1000)
                    .default_value(100)
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
            "model-profile" => settings.profile = value.get().expect("profile property"),
            "score-threshold" => settings.threshold = value.get().expect("float property"),
            "max-detections" => settings.cap = value.get().expect("uint property"),
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
            "model-profile" => settings.profile.to_value(),
            "score-threshold" => settings.threshold.to_value(),
            "max-detections" => settings.cap.to_value(),
            "max-frame-bytes" => settings.maximum.to_value(),
            "execution-provider" => settings.provider.to_value(),
            "coreml-compute-units" => settings.units.to_value(),
            _ => glib::Value::from_type(pspec.value_type()),
        }
    }
}
impl GstObjectImpl for UslsObjectDetect {}
impl ElementImpl for UslsObjectDetect {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "usls Object Detection",
                "Filter/Analysis/Video",
                "Attaches synchronous source-frame detection analytics",
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
impl BaseTransformImpl for UslsObjectDetect {
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
            });
            return Ok(());
        }
        let initialize = || -> Result<State> {
            let settings = self
                .settings
                .lock()
                .map_err(|_error| anyhow::anyhow!("settings lock poisoned"))?
                .clone();
            let path = settings.model.context("model-file is required")?;
            ensure!(
                (1..=128 * 1024 * 1024).contains(&settings.maximum),
                "invalid max-frame-bytes"
            );
            Ok(State {
                backend: Backend::new(
                    &path,
                    settings.profile,
                    settings.provider,
                    settings.units,
                    settings.threshold,
                    usize::try_from(settings.cap)?,
                )?,
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
            .map_err(|error| gst::error_msg!(gst::LibraryError::Init, ["{error:#}"]))?;
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
impl UslsObjectDetect {
    fn process(&self, buffer: &mut gst::BufferRef) -> Result<()> {
        let detections = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_error| anyhow::anyhow!("state lock poisoned"))?;
            let state = guard.as_mut().context("runtime is unavailable")?;
            let info = state.info.as_ref().context("RGB caps are not negotiated")?;
            let result = catch_unwind(AssertUnwindSafe(|| -> Result<Vec<Detection>> {
                let image = frame::image_rgb(buffer, info, state.maximum)?;
                state.backend.run(&image)
            }));
            if let Ok(result) = result {
                result?
            } else {
                // Drop the unusable runtime before returning across the GStreamer boundary.
                let poisoned = guard.take();
                let dropped = catch_unwind(AssertUnwindSafe(|| drop(poisoned)));
                ensure!(dropped.is_ok(), "upstream runtime panicked during discard");
                anyhow::bail!("upstream inference panicked; runtime discarded");
            }
        };
        attach_metadata(buffer, &detections)
    }
}

fn attach_metadata(buffer: &mut gst::BufferRef, detections: &[Detection]) -> Result<()> {
    if detections.is_empty() {
        return Ok(());
    }
    let exists = buffer
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .is_some();
    let mut relation = if exists {
        buffer
            .meta_mut::<gst_analytics::AnalyticsRelationMeta>()
            .context("existing relation meta is not writable")?
    } else {
        gst_analytics::AnalyticsRelationMeta::add(buffer)
    };
    for detection in detections {
        let [x1, y1, x2, y2] = detection.xyxy;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "finite coordinates are clamped to negotiated positive i32 frame bounds"
        )]
        let (x, y, right, bottom) = (
            x1.floor() as i32,
            y1.floor() as i32,
            x2.ceil() as i32,
            y2.ceil() as i32,
        );
        let object = relation
            .add_od_mtd(
                detection.label,
                x,
                y,
                right - x,
                bottom - y,
                detection.score,
            )?
            .id();
        let class = relation
            .add_one_cls_mtd(detection.score, detection.label)?
            .id();
        relation.set_relation(gst_analytics::RelTypes::RELATE_TO, object, class)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::subclass::prelude::ObjectSubclassIsExt;

    fn fake(image: &usls::Image) -> Result<Vec<Detection>> {
        ensure!(
            image.width() == 3 && image.height() == 2,
            "fixture dimensions"
        );
        ensure!(
            image.as_raw() == &(1..=18).collect::<Vec<u8>>(),
            "exact packed pixels must honor VideoMeta offset/stride"
        );
        Ok(vec![
            Detection {
                label: glib::Quark::from_str("person"),
                score: 0.9,
                xyxy: [-1.0, -1.0, 8.0, 8.0],
            },
            Detection {
                label: glib::Quark::from_str("dog"),
                score: 0.7,
                xyxy: [1.0, 0.0, 2.0, 1.0],
            },
        ])
    }
    #[expect(
        clippy::unnecessary_wraps,
        reason = "fixture uses the inference function signature"
    )]
    fn empty(_image: &usls::Image) -> Result<Vec<Detection>> {
        Ok(vec![])
    }
    fn error(_image: &usls::Image) -> Result<Vec<Detection>> {
        anyhow::bail!("fixture inference error")
    }
    #[expect(
        clippy::panic_in_result_fn,
        reason = "private fixture proves panic containment at the GStreamer boundary"
    )]
    fn panicking(_image: &usls::Image) -> Result<Vec<Detection>> {
        panic!("fixture upstream panic")
    }

    fn element(
        infer: fn(&usls::Image) -> Result<Vec<Detection>>,
    ) -> Result<super::super::UslsObjectDetect> {
        gst::init()?;
        let element: super::super::UslsObjectDetect = glib::Object::new();
        *element
            .imp()
            .fixture
            .lock()
            .map_err(|_error| anyhow::anyhow!("fixture lock"))? = Some(Backend::fixture(infer));
        Ok(element)
    }
    fn buffer() -> Result<gst::Buffer> {
        let mut data = vec![99_u8; 5];
        data.extend(1..=9);
        data.extend([99; 7]);
        data.extend(10..=18);
        data.extend([99; 7]);
        let mut buffer = gst::Buffer::from_mut_slice(data);
        let b = buffer.make_mut();
        b.set_pts(gst::ClockTime::from_mseconds(123));
        b.set_dts(gst::ClockTime::from_mseconds(120));
        b.set_duration(gst::ClockTime::from_mseconds(40));
        b.set_offset(17);
        b.set_offset_end(18);
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
        let mut prior = gst_analytics::AnalyticsRelationMeta::add(b);
        prior.add_one_cls_mtd(0.3, glib::Quark::from_str("prior"))?;
        Ok(buffer)
    }
    fn harness(element: &super::super::UslsObjectDetect) -> gst_check::Harness {
        let mut h = gst_check::Harness::with_element(element, Some("sink"), Some("src"));
        h.set_src_caps_str(
            "video/x-raw,format=RGB,width=3,height=2,pixel-aspect-ratio=1/1,framerate=25/1",
        );
        h.play();
        h
    }

    #[test]
    fn model_profile_defaults_and_changes_only_through_ready() -> Result<()> {
        let e = element(empty)?;
        ensure!(
            e.property::<Profile>("model-profile") == Profile::RfdetrNano,
            "RF compatibility default"
        );
        e.set_state(gst::State::Ready)?;
        e.set_property_from_str("model-profile", "rtdetr-v2-s");
        ensure!(
            e.property::<Profile>("model-profile") == Profile::RtdetrV2S,
            "READY mutation"
        );
        {
            let mut h = harness(&e);
            h.push_and_pull(buffer()?)?;
            e.set_property_from_str("model-profile", "picodet-l-coco");
            ensure!(
                e.property::<Profile>("model-profile") == Profile::RtdetrV2S,
                "streaming mutation rejected"
            );
        }
        e.set_property_from_str("model-profile", "deimv2-dinov3-coco");
        ensure!(
            e.property::<Profile>("model-profile") == Profile::Deimv2Dinov3Coco,
            "family selection after stop"
        );
        e.set_property_from_str("model-profile", "deimv2-atto-coco");
        ensure!(
            e.property::<Profile>("model-profile") == Profile::Deimv2AttoCoco,
            "mutation after stop"
        );
        Ok(())
    }

    #[test]
    fn preserves_carrier_and_prior_meta_and_relates_each_detection() -> Result<()> {
        let e = element(fake)?;
        let mut h = harness(&e);
        let input = buffer()?;
        let expected = input.map_readable()?.as_slice().to_vec();
        let output = h.push_and_pull(input)?;
        ensure!(
            output.map_readable()?.as_slice() == expected,
            "fixture assertion failed"
        );
        ensure!(
            output.pts() == Some(gst::ClockTime::from_mseconds(123)),
            "fixture assertion failed"
        );
        ensure!(
            output.dts() == Some(gst::ClockTime::from_mseconds(120)),
            "fixture assertion failed"
        );
        ensure!(
            output.duration() == Some(gst::ClockTime::from_mseconds(40)),
            "fixture assertion failed"
        );
        ensure!(output.offset() == 17, "fixture assertion failed");
        ensure!(output.offset_end() == 18, "fixture assertion failed");
        ensure!(
            output.flags().contains(gst::BufferFlags::DISCONT),
            "fixture assertion failed"
        );
        ensure!(
            output
                .iter_meta::<gst_analytics::AnalyticsRelationMeta>()
                .count()
                == 1,
            "fixture assertion failed"
        );
        let relation = output
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .context("relation meta")?;
        ensure!(relation.len() == 5, "fixture assertion failed");
        let objects = relation
            .iter::<gst_analytics::AnalyticsODMtd>()
            .collect::<Vec<_>>();
        ensure!(objects.len() == 2, "fixture assertion failed");
        let location = objects.first().context("person")?.location()?;
        ensure!(
            (location.x, location.y, location.w, location.h) == (0, 0, 3, 2),
            "fixture assertion failed"
        );
        ensure!(
            relation
                .iter::<gst_analytics::AnalyticsClassificationMtd>()
                .any(|class| class
                    .iterate()
                    .any(|(label, score)| label == glib::Quark::from_str("prior")
                        && (score - 0.3).abs() < f32::EPSILON)),
            "prior classification retained"
        );
        for object in objects {
            ensure!(
                relation
                    .iter::<gst_analytics::AnalyticsClassificationMtd>()
                    .any(|class| relation.relation(object.id(), class.id())
                        == gst_analytics::RelTypes::RELATE_TO
                        && class
                            .iterate()
                            .any(|(label, score)| Some(label) == object.obj_type()
                                && (score - object.confidence_level()).abs() < f32::EPSILON)),
                "fixture assertion failed"
            );
        }
        Ok(())
    }

    #[test]
    fn empty_flush_eos_and_restart_two_elements() -> Result<()> {
        for _ in 0..2 {
            let e = element(empty)?;
            for _ in 0..2 {
                {
                    let mut h = harness(&e);
                    let output = h.push_and_pull(buffer()?)?;
                    ensure!(
                        output
                            .meta::<gst_analytics::AnalyticsRelationMeta>()
                            .context("prior meta")?
                            .len()
                            == 1,
                        "fixture assertion failed"
                    );
                    ensure!(
                        h.push_event(gst::event::FlushStart::new()),
                        "fixture assertion failed"
                    );
                    ensure!(
                        h.push_event(gst::event::FlushStop::new(true)),
                        "fixture assertion failed"
                    );
                    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    ensure!(
                        h.push_event(gst::event::Segment::new(&segment)),
                        "fixture assertion failed"
                    );
                    h.push_and_pull(buffer()?)?;
                    ensure!(
                        h.push_event(gst::event::Eos::new()),
                        "fixture assertion failed"
                    );
                }
                ensure!(
                    e.imp()
                        .state
                        .lock()
                        .map_err(|_error| anyhow::anyhow!("state lock"))?
                        .is_none(),
                    "fixture assertion failed"
                );
                *e.imp()
                    .fixture
                    .lock()
                    .map_err(|_error| anyhow::anyhow!("fixture lock"))? =
                    Some(Backend::fixture(empty));
            }
        }
        Ok(())
    }

    #[test]
    fn inference_errors_and_panics_fail_without_escaping() -> Result<()> {
        for infer in [
            error as fn(&usls::Image) -> Result<Vec<Detection>>,
            panicking,
        ] {
            let e = element(infer)?;
            let mut h = harness(&e);
            ensure!(
                h.push(buffer()?).expect_err("inference fails") == gst::FlowError::Error,
                "fixture assertion failed"
            );
            if std::ptr::fn_addr_eq(
                infer,
                panicking as fn(&usls::Image) -> Result<Vec<Detection>>,
            ) {
                ensure!(
                    e.imp()
                        .state
                        .lock()
                        .map_err(|_error| anyhow::anyhow!("state lock"))?
                        .is_none(),
                    "fixture assertion failed"
                );
                ensure!(
                    h.push(buffer()?).expect_err("discarded runtime") == gst::FlowError::Error,
                    "fixture assertion failed"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn missing_and_corrupt_models_and_invalid_settings_fail_startup() -> Result<()> {
        gst::init()?;
        let e: super::super::UslsObjectDetect = glib::Object::new();
        e.imp().start().expect_err("required model");
        e.set_property("model-file", "/definitely/missing/usls.onnx");
        e.imp().start().expect_err("missing model");
        e.set_property(
            "model-file",
            env!("CARGO_MANIFEST_DIR").to_owned() + "/Cargo.toml",
        );
        e.imp().start().expect_err("corrupt graph");
        e.imp()
            .settings
            .lock()
            .map_err(|_error| anyhow::anyhow!("settings lock"))?
            .threshold = f32::NAN;
        let failure = e.imp().start().expect_err("nonfinite threshold");
        ensure!(
            failure
                .to_string()
                .contains("score-threshold must be finite"),
            "threshold-specific failure: {failure}"
        );
        Ok(())
    }

    #[test]
    fn renegotiation_rejects_changed_format_and_output_geometry() -> Result<()> {
        let e = element(empty)?;
        e.imp().start()?;
        let rgb = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, 3, 2)
            .build()?
            .to_caps()?;
        let changed = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, 4, 2)
            .build()?
            .to_caps()?;
        let bgr = gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgr, 3, 2)
            .build()?
            .to_caps()?;
        e.imp().set_caps(&rgb, &rgb)?;
        e.imp()
            .set_caps(&rgb, &changed)
            .expect_err("geometry must match");
        e.imp().set_caps(&bgr, &bgr).expect_err("format invalid");
        e.imp().stop()?;
        Ok(())
    }
}
