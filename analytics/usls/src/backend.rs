use std::{cell::RefCell, fs::File, io::Read, path::Path, sync::OnceLock};

use anyhow::{Context, Result, ensure};
use gst::glib;
use ort::{logging::LogLevel, value::TensorElementType};
use sha2::{Digest, Sha256};
use usls::{
    Config, Device, Image, ImageTransformInfo, Model, NAMES_COCO_80, OrtTensorAttr, PicoDet,
    RFDETR, RTDETR, ResizeModeType, Runtime, Y,
};

// RF-DETR category IDs: https://github.com/roboflow/rf-detr/blob/fb4ceea0d5750890e43964f5fc558c4caf9b6f38/rfdetr/util/coco_classes.py
const COCO_CATEGORY_IDS: [usize; 80] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 27, 28,
    31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55,
    56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 67, 70, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 84,
    85, 86, 87, 88, 89, 90,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstUslsModelProfile")]
pub(crate) enum Profile {
    #[default]
    #[enum_value(name = "RF-DETR Nano", nick = "rfdetr-nano")]
    RfdetrNano = 0,
    #[enum_value(name = "RT-DETR v1 R18", nick = "rtdetr-v1-r18")]
    RtdetrV1R18 = 1,
    #[enum_value(name = "RT-DETR v2 S", nick = "rtdetr-v2-s")]
    RtdetrV2S = 2,
    #[enum_value(name = "RT-DETR v4 S", nick = "rtdetr-v4-s")]
    RtdetrV4S = 3,
    #[enum_value(name = "PicoDet L COCO", nick = "picodet-l-coco")]
    PicodetLCoco = 4,
    #[enum_value(name = "D-FINE N COCO", nick = "dfine-n-coco")]
    DfineNCoco = 5,
    #[enum_value(name = "DEIM D-FINE S COCO", nick = "deim-dfine-s-coco")]
    DeimDfineSCoco = 6,
    #[enum_value(name = "DEIMv2 Atto COCO", nick = "deimv2-atto-coco")]
    Deimv2AttoCoco = 7,
}

impl Profile {
    fn artifact(self) -> (&'static str, u64, usize) {
        match self {
            Self::RfdetrNano => (
                "9bbecf62d2ac0718cdd9d36cc30fd013d6efdd00e887cd66765ac077bc7177b1",
                108_050_760,
                384,
            ),
            Self::RtdetrV1R18 => (
                "79875f7b608d06e065f50ea473a5887537b93f8abd60c1c9060fd37c8f23a8e4",
                80_464_702,
                640,
            ),
            Self::RtdetrV2S => (
                "c75afbcebb58e1b8016ee6c1a09f879995e4ed273707572e2c5ee9528a26948a",
                80_447_976,
                640,
            ),
            Self::RtdetrV4S => (
                "8b8931caa850d2c9712ca90851bb856a43a3a1a0faf5862567ace7c0b725ddd2",
                41_770_934,
                640,
            ),
            Self::PicodetLCoco => (
                "5255b1a46031f717c7cc1e9ca2c1ac74117f6206e30eba27d4f9aa23ac20ecac",
                23_602_032,
                640,
            ),
            Self::DfineNCoco => (
                "537c995799e8391215de8e7d2e5d3469787ea4ae7e53068751fabd6e2c0db9e3",
                15_501_246,
                640,
            ),
            Self::DeimDfineSCoco => (
                "78345b185cc28238b35cf2ec5721e86f2678948aed5ef7b572c6e59fe7b73efd",
                41_761_973,
                640,
            ),
            Self::Deimv2AttoCoco => (
                "5776c9b6fe8baa74b686d800265e44cb084d1fea4712c6ae43cf8d16ab857b5a",
                2_192_333,
                320,
            ),
        }
    }

    fn config(self) -> Config {
        match self {
            Self::RfdetrNano => Config::rfdetr_nano(),
            Self::RtdetrV1R18 => Config::rtdetr_v1_r18(),
            Self::RtdetrV2S => Config::rtdetr_v2_s(),
            Self::RtdetrV4S => Config::rtdetr_v4_s(),
            Self::PicodetLCoco => Config::picodet_l_coco(),
            Self::DfineNCoco => Config::d_fine_n_coco(),
            Self::DeimDfineSCoco => Config::deim_dfine_s_coco(),
            Self::Deimv2AttoCoco => Config::deim_v2_atto_coco(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstUslsExecutionProvider")]
pub(crate) enum Provider {
    #[default]
    Cpu = 0,
    #[enum_value(name = "CoreML", nick = "coreml")]
    Coreml = 1,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstUslsCoreMLComputeUnits")]
pub(crate) enum ComputeUnits {
    All = 0,
    #[default]
    #[enum_value(name = "CPU and GPU", nick = "cpu-and-gpu")]
    CpuAndGpu = 1,
    #[enum_value(name = "CPU and Neural Engine", nick = "cpu-and-neural-engine")]
    CpuAndNeuralEngine = 2,
}

#[derive(Clone, Debug)]
pub(crate) struct Detection {
    pub label: glib::Quark,
    pub score: f32,
    pub xyxy: [f64; 4],
}

thread_local! {
    // ORT's final placement is emitted synchronously by the initializing thread.
    // Other threads and later sessions cannot supply evidence for this scope.
    static INITIALIZATION: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct Capture;
impl Capture {
    fn begin() -> Result<Self> {
        INITIALIZATION.with(|messages| {
            ensure!(messages.borrow().is_none(), "nested ORT initialization");
            *messages.borrow_mut() = Some(Vec::new());
            Ok(Self)
        })
    }
    #[expect(
        clippy::unused_self,
        reason = "consumes the capture guard to close its initialization scope"
    )]
    fn finish(self) -> Vec<String> {
        INITIALIZATION.with(|messages| messages.borrow_mut().take().unwrap_or_default())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        INITIALIZATION.with(|messages| *messages.borrow_mut() = None);
    }
}

fn logger_owned() -> Result<bool> {
    static OWNED: OnceLock<std::result::Result<bool, String>> = OnceLock::new();
    OWNED
        .get_or_init(|| {
            let owned = ort::init()
                .with_logger(std::sync::Arc::new(
                    |_level, _category, _id, location, message| {
                        if location.contains("VerifyEachNodeIsAssignedToAnEp") {
                            INITIALIZATION.with(|messages| {
                                if let Ok(mut messages) = messages.try_borrow_mut()
                                    && let Some(messages) = messages.as_mut()
                                {
                                    messages.push(message.to_owned());
                                }
                            });
                        }
                    },
                ))
                .commit();
            if owned {
                ort::environment::Environment::current()
                    .map_err(|error| error.to_string())?
                    .set_log_level(LogLevel::Verbose);
            }
            Ok(owned)
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

pub(crate) fn verify_assignment(provider: Provider, messages: &[String]) -> Result<()> {
    if provider == Provider::Coreml {
        ensure!(
            messages
                .iter()
                .any(|line| line.contains("Node(s) placed on [CoreMLExecutionProvider]")),
            "CoreML requested but no final CoreML assignment verified; all-CPU fallback rejected"
        );
    }
    Ok(())
}

pub(crate) fn verify_model(path: &Path, profile: Profile) -> Result<()> {
    let (sha256, length, _) = profile.artifact();
    ensure!(path.is_file(), "model-file must name a local regular file");
    let mut file = File::open(path).context("opening model-file")?;
    ensure!(
        file.metadata()?.len() == length,
        "model-file length differs from the qualified {profile:?} graph (expected {length} bytes)"
    );
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 8192];
    loop {
        let count = file.read(&mut bytes).context("reading model-file")?;
        if count == 0 {
            break;
        }
        hash.update(bytes.get(..count).context("invalid file read length")?);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == sha256,
        "model-file is not the qualified {profile:?} FP32 batch1 graph (expected SHA256 {sha256})"
    );
    Ok(())
}

fn verify_units(profile: Profile, provider: Provider, units: ComputeUnits) -> Result<()> {
    ensure!(
        provider != Provider::Coreml
            || profile == Profile::RfdetrNano
            || units == ComputeUnits::CpuAndGpu,
        "CoreML {units:?} is unqualified for {profile:?}; use cpu-and-gpu"
    );
    Ok(())
}

fn verify_contract(
    profile: Profile,
    inputs: &OrtTensorAttr,
    outputs: &OrtTensorAttr,
) -> Result<()> {
    let (_, _, size) = profile.artifact();
    let inputs_valid = match profile {
        Profile::RfdetrNano => {
            inputs.names == ["images"]
                && inputs.dimss == [vec![1, 3, size, size]]
                && inputs.dtypes == [TensorElementType::Float32]
        }
        Profile::PicodetLCoco => {
            inputs.names == ["image", "scale_factor"]
                && inputs.dimss == [vec![1, 3, size, size], vec![1, 2]]
                && inputs.dtypes == [TensorElementType::Float32, TensorElementType::Float32]
        }
        _ => {
            inputs.names == ["images", "orig_target_sizes"]
                && inputs.dimss == [vec![1, 3, size, size], vec![1, 2]]
                && inputs.dtypes == [TensorElementType::Float32, TensorElementType::Int64]
        }
    };
    let outputs_valid = match profile {
        Profile::RfdetrNano => {
            outputs.names == ["pred_boxes", "pred_logits"]
                && outputs.dimss == [vec![1, 0, 4], vec![1, 0, 91]]
                && outputs.dtypes == [TensorElementType::Float32, TensorElementType::Float32]
        }
        Profile::PicodetLCoco => {
            outputs.names == ["multiclass_nms3_0.tmp_0", "multiclass_nms3_0.tmp_2"]
                && outputs.dimss == [vec![0, 6], vec![1]]
                && outputs.dtypes == [TensorElementType::Float32, TensorElementType::Int32]
        }
        _ => {
            let batch = usize::from(!matches!(
                profile,
                Profile::DfineNCoco | Profile::DeimDfineSCoco
            ));
            outputs.names == ["labels", "boxes", "scores"]
                && outputs.dimss == [vec![batch, 300], vec![batch, 300, 4], vec![batch, 300]]
                && outputs.dtypes
                    == [
                        TensorElementType::Int64,
                        TensorElementType::Float32,
                        TensorElementType::Float32,
                    ]
        }
    };
    ensure!(
        inputs_valid && outputs_valid,
        "qualified {profile:?} model tensor contract mismatch"
    );
    Ok(())
}

pub(crate) struct Backend {
    runtime: Inference,
    profile: Profile,
    threshold: f32,
    cap: usize,
}

enum Inference {
    Rfdetr(Box<Runtime<RFDETR>>),
    Rtdetr(Box<Runtime<RTDETR>>),
    Picodet(Box<Runtime<PicoDet>>),
    #[cfg(test)]
    Fixture(fn(&Image) -> Result<Vec<Detection>>),
}

impl Backend {
    pub fn new(
        path: &Path,
        profile: Profile,
        provider: Provider,
        units: ComputeUnits,
        threshold: f32,
        cap: usize,
    ) -> Result<Self> {
        ensure!(
            threshold.is_finite() && (0.0..=1.0).contains(&threshold),
            "score-threshold must be finite in 0..1"
        );
        ensure!((1..=1000).contains(&cap), "invalid max-detections");
        ensure!(
            provider != Provider::Coreml
                || cfg!(all(
                    feature = "coreml",
                    target_os = "macos",
                    target_arch = "aarch64"
                )),
            "CoreML is unavailable: requires the coreml feature on Apple Silicon macOS"
        );
        verify_units(profile, provider, units)?;
        let path = path.canonicalize().context("resolving local model-file")?;
        verify_model(&path, profile)?;
        let owned = logger_owned()?;
        ensure!(
            provider != Provider::Coreml || owned,
            "CoreML assignment cannot be verified: ORT was initialized externally without the usls logger; CPU remains available"
        );
        let capture = Capture::begin()?;
        let config = profile
            .config()
            .with_model_file(path.to_str().context("model-file must be UTF-8")?)
            .with_model_batch_size_min_opt_max(1, 1, 1)
            .with_model_num_dry_run(0)
            .with_model_num_intra_threads(10)
            .with_model_num_inter_threads(1)
            .with_model_device(if provider == Provider::Cpu {
                Device::Cpu(0)
            } else {
                Device::CoreMl
            })
            .with_class_confs(&[threshold])
            .with_model_coreml_static_input_shapes(true)
            .with_model_coreml_subgraph_running(true)
            .with_model_coreml_model_format(0)
            .with_model_coreml_specialization_strategy(1)
            .with_model_coreml_compute_units(units as u8)
            .commit()?;
        let runtime = match profile {
            Profile::RfdetrNano => Inference::Rfdetr(Box::new(RFDETR::new(config)?)),
            Profile::PicodetLCoco => Inference::Picodet(Box::new(PicoDet::new(config)?)),
            _ => Inference::Rtdetr(Box::new(RTDETR::new(config)?)),
        };
        let assignment = capture.finish();
        verify_assignment(provider, &assignment)?;
        let engine = match &runtime {
            Inference::Rfdetr(runtime) => runtime.engines().get(&usls::Module::Model),
            Inference::Rtdetr(runtime) => runtime.engines().get(&usls::Module::Model),
            Inference::Picodet(runtime) => runtime.engines().get(&usls::Module::Model),
            #[cfg(test)]
            Inference::Fixture(_) => None,
        }
        .context("missing engine")?;
        verify_contract(profile, &engine.inputs, &engine.outputs)?;
        Ok(Self {
            runtime,
            profile,
            threshold,
            cap,
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture(infer: fn(&Image) -> Result<Vec<Detection>>) -> Self {
        Self {
            runtime: Inference::Fixture(infer),
            profile: Profile::RfdetrNano,
            threshold: 0.5,
            cap: 100,
        }
    }

    pub fn run(&mut self, image: &Image) -> Result<Vec<Detection>> {
        let results = match &mut self.runtime {
            Inference::Rfdetr(runtime) => {
                // Upstream clips the origin before preserving width/height, shifting edge boxes.
                let (model, engines) = runtime.parts_mut();
                ensure!(
                    matches!(
                        model.processor.resize_mode_type(),
                        Some(ResizeModeType::Letterbox)
                    ),
                    "RF-DETR requires the qualified Letterbox preset"
                );
                let inputs = model.processor.process(std::slice::from_ref(image))?;
                let outputs = engines.run(&usls::Module::Model, &inputs)?.into_inner();
                let boxes = outputs
                    .get("pred_boxes")
                    .context("missing RF-DETR boxes")?
                    .try_extract_array::<f32>()
                    .context("RF-DETR boxes must be FP32 CPU-accessible tensors")?;
                let logits = outputs
                    .get("pred_logits")
                    .context("missing RF-DETR logits")?
                    .try_extract_array::<f32>()
                    .context("RF-DETR logits must be FP32 CPU-accessible tensors")?;
                ensure!(
                    model.processor.images_transform_info.len() == 1,
                    "unexpected RF-DETR transform count"
                );
                return decode_rfdetr(
                    (
                        boxes.shape(),
                        boxes.as_slice().context("noncontiguous RF-DETR boxes")?,
                    ),
                    (
                        logits.shape(),
                        logits.as_slice().context("noncontiguous RF-DETR logits")?,
                    ),
                    [model.width, model.height],
                    model
                        .processor
                        .images_transform_info
                        .first()
                        .context("missing RF-DETR transform")?,
                    self.threshold,
                    self.cap,
                );
            }
            Inference::Rtdetr(runtime) => runtime.run(std::slice::from_ref(image))?,
            Inference::Picodet(runtime) => runtime.run(std::slice::from_ref(image))?,
            #[cfg(test)]
            Inference::Fixture(infer) => {
                return sanitize(
                    infer(image)?,
                    image.width(),
                    image.height(),
                    self.threshold,
                    self.cap,
                );
            }
        };
        decode(
            self.profile,
            results,
            image.width(),
            image.height(),
            self.threshold,
            self.cap,
        )
    }
}

fn decode_rfdetr(
    (box_shape, boxes): (&[usize], &[f32]),
    (logit_shape, logits): (&[usize], &[f32]),
    [width, height]: [usize; 2],
    info: &ImageTransformInfo,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    let queries = match (box_shape, logit_shape) {
        ([1, boxes, 4], [1, logits, 91]) if boxes == logits => *boxes,
        _ => anyhow::bail!("unexpected RF-DETR tensor shapes: {box_shape:?}, {logit_shape:?}"),
    };
    ensure!(
        queries.checked_mul(4) == Some(boxes.len())
            && queries.checked_mul(91) == Some(logits.len()),
        "RF-DETR tensor lengths differ from shapes"
    );
    ensure!(
        boxes.iter().chain(logits).all(|value| value.is_finite()),
        "nonfinite RF-DETR output"
    );
    ensure!(
        width > 0
            && height > 0
            && info.width_src > 0
            && info.height_src > 0
            && info.height_scale.is_finite()
            && info.height_scale > 0.0
            && info.width_pad.is_finite()
            && info.height_pad.is_finite(),
        "invalid RF-DETR letterbox geometry"
    );
    let width = f32::from(u16::try_from(width).context("RF-DETR model width exceeds bound")?);
    let height = f32::from(u16::try_from(height).context("RF-DETR model height exceeds bound")?);
    let mut detections = Vec::new();
    let mut classes = logits.as_chunks::<91>().0.iter();
    for [cx, cy, w, h] in boxes.as_chunks::<4>().0 {
        let (id, logit) = classes
            .next()
            .context("missing RF-DETR query logits")?
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .context("missing RF-DETR classes")?;
        let score = 1.0 / ((-logit).exp() + 1.0);
        if score < threshold {
            continue;
        }
        let cx = (cx * width - info.width_pad) / info.height_scale;
        let cy = (cy * height - info.height_pad) / info.height_scale;
        let w = w * width / info.height_scale;
        let h = h * height / info.height_scale;
        // Keep the original endpoints before sanitize clips either edge.
        let x1 = cx - w / 2.0;
        let y1 = cy - h / 2.0;
        detections.push(Detection {
            label: glib::Quark::from_str(class_label(Profile::RfdetrNano, id)?),
            score,
            xyxy: [x1, y1, x1 + w, y1 + h].map(f64::from),
        });
    }
    sanitize(detections, info.width_src, info.height_src, threshold, cap)
}

fn decode(
    profile: Profile,
    results: Vec<Y>,
    width: u32,
    height: u32,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    ensure!(
        results.len() == 1,
        "batch1 returned an unexpected number of results"
    );
    let result = results.into_iter().next().context("missing result")?;
    let mut boxes = Vec::new();
    for bbox in result.hbbs {
        let label = class_label(profile, bbox.id().context("missing class")?)?;
        let (x1, y1, x2, y2) = bbox.xyxy();
        boxes.push(Detection {
            label: glib::Quark::from_str(label),
            score: bbox.confidence().context("missing score")?,
            xyxy: [f64::from(x1), f64::from(y1), f64::from(x2), f64::from(y2)],
        });
    }
    sanitize(boxes, width, height, threshold, cap)
}

pub(crate) fn sanitize(
    boxes: Vec<Detection>,
    width: u32,
    height: u32,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    let mut result = Vec::new();
    for mut bbox in boxes {
        ensure!(
            bbox.score.is_finite() && bbox.xyxy.iter().all(|value| value.is_finite()),
            "nonfinite detection"
        );
        ensure!((0.0..=1.0).contains(&bbox.score), "invalid confidence");
        let [x1, y1, x2, y2] = bbox.xyxy;
        bbox.xyxy = [
            x1.clamp(0.0, f64::from(width)),
            y1.clamp(0.0, f64::from(height)),
            x2.clamp(0.0, f64::from(width)),
            y2.clamp(0.0, f64::from(height)),
        ];
        let [x1, y1, x2, y2] = bbox.xyxy;
        if bbox.score >= threshold && x2 > x1 && y2 > y1 {
            result.push(bbox);
        }
    }
    result.sort_by(|a, b| b.score.total_cmp(&a.score));
    result.truncate(cap);
    Ok(result)
}

fn class_label(profile: Profile, id: usize) -> Result<&'static str> {
    let index = if profile == Profile::RfdetrNano {
        COCO_CATEGORY_IDS
            .binary_search(&id)
            .map_err(|_error| anyhow::anyhow!("unexpected RF-DETR background/gap class {id}"))?
    } else {
        id
    };
    NAMES_COCO_80
        .get(index)
        .copied()
        .context("unknown COCO80 class")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfdetr_preserves_endpoints_across_letterbox_edges() -> Result<()> {
        for info in [
            ImageTransformInfo {
                width_src: 400,
                height_src: 200,
                height_scale: 0.5,
                height_pad: 50.0,
                ..ImageTransformInfo::default()
            },
            ImageTransformInfo {
                width_src: 200,
                height_src: 400,
                height_scale: 0.5,
                width_pad: 50.0,
                ..ImageTransformInfo::default()
            },
        ] {
            let width = f32::from(u16::try_from(info.width_src)?);
            let height = f32::from(u16::try_from(info.height_src)?);
            let endpoints = [
                [20.0, 30.0, 80.0, 90.0],
                [-10.0, 20.0, 30.0, 60.0], // Upstream shifted the right endpoint to 40.
                [20.0, -10.0, 60.0, 30.0],
                [width - 30.0, 20.0, width + 10.0, 60.0],
                [20.0, height - 30.0, 60.0, height + 10.0],
                [-50.0, 20.0, -10.0, 60.0],
            ];
            let mut boxes = Vec::new();
            let mut logits = Vec::new();
            for [x1, y1, x2, y2] in endpoints {
                boxes.extend([
                    (f32::midpoint(x1, x2) * info.height_scale + info.width_pad) / 200.0,
                    (f32::midpoint(y1, y2) * info.height_scale + info.height_pad) / 200.0,
                    (x2 - x1) * info.height_scale / 200.0,
                    (y2 - y1) * info.height_scale / 200.0,
                ]);
                let mut classes = [-10.0; 91];
                *classes.get_mut(1).context("person logit")? = 2.0;
                logits.extend(classes);
            }
            let detections = decode_rfdetr(
                (&[1, 6, 4], &boxes),
                (&[1, 6, 91], &logits),
                [200, 200],
                &info,
                0.5,
                100,
            )?;
            ensure!(detections.len() == 5, "fully outside box must be dropped");
            for (bbox, [x1, y1, x2, y2]) in detections.iter().zip(endpoints) {
                let expected = [
                    x1.clamp(0.0, width),
                    y1.clamp(0.0, height),
                    x2.clamp(0.0, width),
                    y2.clamp(0.0, height),
                ];
                ensure!(
                    bbox.xyxy
                        .iter()
                        .zip(expected)
                        .all(|(actual, expected)| (*actual - f64::from(expected)).abs() < 0.0001),
                    "original endpoints: {:?} vs {expected:?}",
                    bbox.xyxy
                );
                ensure!(
                    bbox.label.as_str() == "person"
                        && bbox.score.to_bits() == (1.0 / ((-2.0_f32).exp() + 1.0)).to_bits(),
                    "class and score unchanged"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn rfdetr_rejects_malformed_outputs_and_preserves_argmax_ties() -> Result<()> {
        let info = ImageTransformInfo {
            width_src: 200,
            height_src: 200,
            height_scale: 1.0,
            ..ImageTransformInfo::default()
        };
        let boxes = [0.5, 0.5, 0.2, 0.2];
        let mut logits = [-10.0; 91];
        *logits.get_mut(1).context("person")? = 2.0;
        *logits.get_mut(2).context("bicycle")? = 2.0;
        let run = |box_shape: &[usize], boxes: &[f32], logit_shape: &[usize], logits: &[f32]| {
            decode_rfdetr(
                (box_shape, boxes),
                (logit_shape, logits),
                [200, 200],
                &info,
                0.5,
                100,
            )
        };
        let decoded = run(&[1, 1, 4], &boxes, &[1, 1, 91], &logits)?;
        ensure!(
            decoded.first().context("tie result")?.label.as_str() == "bicycle",
            "upstream argmax chooses last tie"
        );
        for shape in [vec![1, 4], vec![2, 1, 4], vec![1, 1, 5], vec![1, 2, 4]] {
            run(&shape, &boxes, &[1, 1, 91], &logits).expect_err("invalid boxes shape");
        }
        for shape in [vec![1, 91], vec![2, 1, 91], vec![1, 1, 90], vec![1, 2, 91]] {
            run(&[1, 1, 4], &boxes, &shape, &logits).expect_err("invalid logits shape");
        }
        run(&[1, 1, 4], &[], &[1, 1, 91], &logits).expect_err("incomplete box data");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut invalid = boxes;
            *invalid.first_mut().context("box")? = bad;
            run(&[1, 1, 4], &invalid, &[1, 1, 91], &logits).expect_err("nonfinite box");
            let mut invalid = logits;
            *invalid.first_mut().context("logit")? = bad;
            run(&[1, 1, 4], &boxes, &[1, 1, 91], &invalid).expect_err("nonfinite logit");
        }
        *logits.get_mut(83).context("gap")? = 3.0;
        run(&[1, 1, 4], &boxes, &[1, 1, 91], &logits).expect_err("retained gap class");
        Ok(())
    }

    #[test]
    fn coco_gaps_are_rejected() -> Result<()> {
        ensure!(
            COCO_CATEGORY_IDS
                .windows(2)
                .all(|ids| matches!(ids, [left, right] if left < right)),
            "binary search requires sorted unique category IDs"
        );
        for id in [0, 12, 26, 29, 30, 45, 66, 68, 69, 71, 83, 91, usize::MAX] {
            class_label(Profile::RfdetrNano, id).expect_err("background, gap or out of range");
        }
        ensure!(
            class_label(Profile::RfdetrNano, 13)? == "stop sign",
            "fixture assertion failed"
        );
        ensure!(
            class_label(Profile::RfdetrNano, 90)? == "toothbrush",
            "fixture assertion failed"
        );
        Ok(())
    }

    const PROFILES: [Profile; 8] = [
        Profile::RfdetrNano,
        Profile::RtdetrV1R18,
        Profile::RtdetrV2S,
        Profile::RtdetrV4S,
        Profile::PicodetLCoco,
        Profile::DfineNCoco,
        Profile::DeimDfineSCoco,
        Profile::Deimv2AttoCoco,
    ];

    #[test]
    fn labels_cover_dense_and_sparse_coco() -> Result<()> {
        for profile in PROFILES {
            let ids = if profile == Profile::RfdetrNano {
                [1, 13, 84, 90]
            } else {
                [0, 11, 73, 79]
            };
            for (id, name) in ids
                .into_iter()
                .zip(["person", "stop sign", "book", "toothbrush"])
            {
                ensure!(
                    class_label(profile, id)? == name,
                    "class mapping for {profile:?}"
                );
            }
            class_label(profile, 91).expect_err("out of range");
            class_label(profile, usize::MAX).expect_err("out of range");
            if profile != Profile::RfdetrNano {
                class_label(profile, 80).expect_err("dense COCO80 upper bound");
            }
        }
        Ok(())
    }

    #[test]
    fn profiles_reject_unqualified_units_and_model_files() -> Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut hashes = std::collections::HashSet::new();
        for profile in PROFILES {
            ensure!(
                hashes.insert(profile.artifact().0),
                "each profile pins its own graph"
            );
            ensure!(
                verify_model(&path, profile)
                    .expect_err("wrong model/profile")
                    .to_string()
                    .contains("length differs"),
                "reject before session initialization"
            );
            verify_units(profile, Provider::Coreml, ComputeUnits::CpuAndGpu)?;
            for units in [ComputeUnits::All, ComputeUnits::CpuAndNeuralEngine] {
                verify_units(profile, Provider::Cpu, units)?;
                if profile == Profile::RfdetrNano {
                    verify_units(profile, Provider::Coreml, units)?;
                } else {
                    ensure!(
                        verify_units(profile, Provider::Coreml, units)
                            .expect_err("unqualified units")
                            .to_string()
                            .contains("unqualified"),
                        "explicit provider rejection"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn same_length_model_corruption_is_rejected_by_hash() -> Result<()> {
        let profile = Profile::Deimv2AttoCoco;
        let path = std::env::temp_dir().join(format!(
            "gstsmith-usls-wrong-hash-{}-{}.onnx",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let file = File::options().write(true).create_new(true).open(&path)?;
        let result = (|| -> Result<()> {
            file.set_len(profile.artifact().1)?;
            let error = verify_model(&path, profile)
                .err()
                .context("same-length corrupt model must fail")?;
            ensure!(
                error.to_string().contains("expected SHA256"),
                "same-length corruption must reach the SHA256 check: {error}"
            );
            Ok(())
        })();
        drop(file);
        std::fs::remove_file(&path)?;
        result
    }

    fn attrs(
        names: &[&str],
        dimss: Vec<Vec<usize>>,
        dtypes: Vec<TensorElementType>,
    ) -> OrtTensorAttr {
        OrtTensorAttr {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            dimss,
            dtypes,
            ..OrtTensorAttr::default()
        }
    }

    #[test]
    fn model_specific_tensor_contracts_reject_mismatches() -> Result<()> {
        use TensorElementType::{Float32, Int32, Int64};
        // Observed contracts of the exact qualified graphs; no model assets required.
        for (profile, size, batch) in [
            (Profile::RfdetrNano, 384, 1),
            (Profile::RtdetrV1R18, 640, 1),
            (Profile::RtdetrV2S, 640, 1),
            (Profile::RtdetrV4S, 640, 1),
            (Profile::PicodetLCoco, 640, 1),
            (Profile::DfineNCoco, 640, 0),
            (Profile::DeimDfineSCoco, 640, 0),
            (Profile::Deimv2AttoCoco, 320, 1),
        ] {
            let (inputs, outputs) = match profile {
                Profile::RfdetrNano => (
                    attrs(&["images"], vec![vec![1, 3, 384, 384]], vec![Float32]),
                    attrs(
                        &["pred_boxes", "pred_logits"],
                        vec![vec![1, 0, 4], vec![1, 0, 91]],
                        vec![Float32, Float32],
                    ),
                ),
                Profile::PicodetLCoco => (
                    attrs(
                        &["image", "scale_factor"],
                        vec![vec![1, 3, 640, 640], vec![1, 2]],
                        vec![Float32, Float32],
                    ),
                    attrs(
                        &["multiclass_nms3_0.tmp_0", "multiclass_nms3_0.tmp_2"],
                        vec![vec![0, 6], vec![1]],
                        vec![Float32, Int32],
                    ),
                ),
                _ => (
                    attrs(
                        &["images", "orig_target_sizes"],
                        vec![vec![1, 3, size, size], vec![1, 2]],
                        vec![Float32, Int64],
                    ),
                    attrs(
                        &["labels", "boxes", "scores"],
                        vec![vec![batch, 300], vec![batch, 300, 4], vec![batch, 300]],
                        vec![Int64, Float32, Float32],
                    ),
                ),
            };
            verify_contract(profile, &inputs, &outputs)?;
            let mut wrong = inputs.clone();
            *wrong
                .dimss
                .first_mut()
                .context("image input")?
                .first_mut()
                .context("batch")? = 2;
            verify_contract(profile, &wrong, &outputs).expect_err("batch mismatch");
            let mut wrong = inputs.clone();
            *wrong.dtypes.first_mut().context("image dtype")? = TensorElementType::Float16;
            verify_contract(profile, &wrong, &outputs).expect_err("precision mismatch");
            let mut wrong = outputs.clone();
            *wrong.names.first_mut().context("output name")? = "unqualified".into();
            verify_contract(profile, &inputs, &wrong).expect_err("output mismatch");
            let different = if profile == Profile::RfdetrNano {
                Profile::RtdetrV2S
            } else {
                Profile::RfdetrNano
            };
            verify_contract(different, &inputs, &outputs).expect_err("wrong profile contract");
        }
        Ok(())
    }

    fn detection(score: f32, xyxy: [f64; 4]) -> Detection {
        Detection {
            label: glib::Quark::from_str("person"),
            score,
            xyxy,
        }
    }

    #[test]
    fn validates_clamps_sorts_and_caps_boxes() -> Result<()> {
        let boxes = vec![
            detection(0.6, [-1.0, -2.0, 50.0, 60.0]),
            detection(0.8, [1.0, 1.0, 2.0, 2.0]),
            detection(0.9, [3.0, 3.0, 3.0, 4.0]),
        ];
        let result = sanitize(boxes, 20, 30, 0.5, 1)?;
        ensure!(result.len() == 1, "fixture assertion failed");
        ensure!(
            result
                .first()
                .context("box")?
                .xyxy
                .iter()
                .zip([1.0, 1.0, 2.0, 2.0])
                .all(|(a, b)| (a - b).abs() < f64::EPSILON),
            "fixture assertion failed"
        );
        let result = sanitize(
            vec![detection(0.6, [-1.0, -2.0, 50.0, 60.0])],
            20,
            30,
            0.5,
            100,
        )?;
        ensure!(
            result
                .first()
                .context("box")?
                .xyxy
                .iter()
                .zip([0.0, 0.0, 20.0, 30.0])
                .all(|(a, b)| (a - b).abs() < f64::EPSILON),
            "fixture assertion failed"
        );
        for bad in [
            detection(f32::NAN, [0.0; 4]),
            detection(1.1, [0.0; 4]),
            detection(0.7, [f64::INFINITY; 4]),
        ] {
            sanitize(vec![bad], 20, 30, 0.5, 100).expect_err("invalid result must fail");
        }
        Ok(())
    }

    #[test]
    fn placement_capture_never_reuses_another_session() -> Result<()> {
        let first = Capture::begin()?;
        INITIALIZATION.with(|messages| {
            *messages.borrow_mut() =
                Some(vec!["1 Node(s) placed on [CoreMLExecutionProvider]".into()]);
        });
        verify_assignment(Provider::Coreml, &first.finish())?;
        let second = Capture::begin()?;
        std::thread::spawn(|| {
            INITIALIZATION.with(|messages| {
                *messages.borrow_mut() =
                    Some(vec!["1 Node(s) placed on [CoreMLExecutionProvider]".into()]);
            });
        })
        .join()
        .map_err(|_error| anyhow::anyhow!("test thread panicked"))?;
        verify_assignment(Provider::Coreml, &second.finish())
            .expect_err("other thread and old session cannot prove placement");
        let aborted = Capture::begin()?;
        drop(aborted);
        verify_assignment(Provider::Coreml, &Capture::begin()?.finish())
            .expect_err("aborted capture cleared");
        verify_assignment(Provider::Cpu, &[])?;
        Ok(())
    }
}
