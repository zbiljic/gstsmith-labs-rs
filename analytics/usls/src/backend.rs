use std::{cell::RefCell, fs::File, path::Path, sync::OnceLock};

use anyhow::{Context, Result, ensure};
use gst::glib;
use ort::{logging::LogLevel, value::TensorElementType};
use usls::{
    Config, Device, Image, ImageTransformInfo, Model, NAMES_COCO_80, OrtTensorAttr, PicoDet,
    RFDETR, RTDETR, ResizeModeType, Runtime, X,
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
    #[enum_value(name = "RF-DETR COCO", nick = "rfdetr-coco")]
    RfdetrCoco = 8,
    #[enum_value(name = "RT-DETR v1 COCO", nick = "rtdetr-v1-coco")]
    RtdetrV1Coco = 9,
    #[enum_value(name = "RT-DETR v2 COCO", nick = "rtdetr-v2-coco")]
    RtdetrV2Coco = 10,
    #[enum_value(name = "RT-DETR v4 COCO", nick = "rtdetr-v4-coco")]
    RtdetrV4Coco = 11,
    #[enum_value(name = "PicoDet COCO", nick = "picodet-coco")]
    PicodetCoco = 12,
    #[enum_value(name = "D-FINE COCO", nick = "dfine-coco")]
    DfineCoco = 13,
    #[enum_value(name = "DEIM D-FINE COCO", nick = "deim-dfine-coco")]
    DeimDfineCoco = 14,
    #[enum_value(name = "DEIMv2 HGNetv2 COCO", nick = "deimv2-hgnetv2-coco")]
    Deimv2Hgnetv2Coco = 15,
    #[enum_value(name = "DEIMv2 DINOv3 COCO", nick = "deimv2-dinov3-coco")]
    Deimv2Dinov3Coco = 16,
}

impl Profile {
    fn is_rfdetr(self) -> bool {
        matches!(self, Self::RfdetrNano | Self::RfdetrCoco)
    }

    fn is_picodet(self) -> bool {
        matches!(self, Self::PicodetLCoco | Self::PicodetCoco)
    }

    fn config(self) -> Config {
        match self {
            Self::RfdetrNano | Self::RfdetrCoco => Config::rfdetr(),
            Self::RtdetrV1R18 | Self::RtdetrV1Coco => Config::rtdetr().with_version(1.into()),
            Self::RtdetrV2S | Self::RtdetrV2Coco => Config::rtdetr().with_version(2.into()),
            Self::RtdetrV4S | Self::RtdetrV4Coco => Config::rtdetr().with_version(4.into()),
            Self::PicodetLCoco | Self::PicodetCoco => {
                Config::picodet().with_class_names(&NAMES_COCO_80)
            }
            Self::DfineNCoco | Self::DfineCoco => Config::d_fine(),
            Self::DeimDfineSCoco | Self::DeimDfineCoco => Config::deim(),
            Self::Deimv2AttoCoco | Self::Deimv2Hgnetv2Coco => Config::deimv2(),
            // The DINOv3 exports require ImageNet normalization; HGNetv2 does not.
            Self::Deimv2Dinov3Coco => Config::deimv2()
                .with_image_mean([0.485, 0.456, 0.406])
                .with_image_std([0.229, 0.224, 0.225]),
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

fn verify_model(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "model-file must name a local regular file");
    File::open(path).context("opening model-file")?;
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
    let image_valid = matches!(
        inputs.dimss.first().map(Vec::as_slice),
        Some([1, 3, height, width]) if *height > 0 && u16::try_from(*height).is_ok() && height == width
    );
    let inputs_valid = if profile.is_rfdetr() {
        inputs.names == ["images"]
            && inputs.dimss.len() == 1
            && inputs.dtypes == [TensorElementType::Float32]
    } else {
        let (names, dtype) = if profile.is_picodet() {
            (["image", "scale_factor"], TensorElementType::Float32)
        } else {
            (["images", "orig_target_sizes"], TensorElementType::Int64)
        };
        inputs.names == names
            && inputs.dimss.len() == 2
            && inputs.dimss.get(1).is_some_and(|shape| shape == &[1, 2])
            && inputs.dtypes == [TensorElementType::Float32, dtype]
    };
    let outputs_valid = if profile.is_rfdetr() {
        (outputs.names == ["pred_boxes", "pred_logits"] || outputs.names == ["boxes", "logits"])
            && matches!(outputs.dimss.as_slice(), [boxes, logits]
                if matches!((boxes.as_slice(), logits.as_slice()),
                    ([b1, q1, coords], [b2, q2, classes])
                    if *b1 <= 1 && *b2 <= 1 && [0, 4].contains(coords)
                        && [0, 91].contains(classes) && compatible_dimensions(&[*q1, *q2])))
            && outputs.dtypes == [TensorElementType::Float32, TensorElementType::Float32]
    } else if profile.is_picodet() {
        outputs.names == ["multiclass_nms3_0.tmp_0", "multiclass_nms3_0.tmp_2"]
            && matches!(outputs.dimss.as_slice(), [rows, count]
                if matches!(rows.as_slice(), [_, columns] if [0, 6].contains(columns))
                    && matches!(count.as_slice(), [batch] if *batch <= 1))
            && outputs.dtypes == [TensorElementType::Float32, TensorElementType::Int32]
    } else {
        outputs.names == ["labels", "boxes", "scores"]
            && matches!(outputs.dimss.as_slice(), [labels, boxes, scores]
                if matches!((labels.as_slice(), boxes.as_slice(), scores.as_slice()),
                    ([b1, q1], [b2, q2, coords], [b3, q3])
                    if *b1 <= 1 && *b2 <= 1 && *b3 <= 1
                        && [0, 4].contains(coords) && compatible_dimensions(&[*q1, *q2, *q3])))
            && outputs.dtypes
                == [
                    TensorElementType::Int64,
                    TensorElementType::Float32,
                    TensorElementType::Float32,
                ]
    };
    ensure!(
        image_valid && inputs_valid && outputs_valid,
        "{profile:?} tensor contract mismatch: expected static batch1 square FP32 NCHW input (size 1..65535) and compatible COCO detection outputs; inputs={inputs:?}, outputs={outputs:?}"
    );
    Ok(())
}

fn compatible_dimensions(dimensions: &[usize]) -> bool {
    // ORT reports symbolic dimensions as zero; validate their actual sizes at inference.
    let mut known = dimensions
        .iter()
        .copied()
        .filter(|dimension| *dimension != 0);
    let first = known.next();
    known.all(|dimension| Some(dimension) == first)
}

pub(crate) struct Backend {
    runtime: Inference,
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
        verify_model(&path)?;
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
            Profile::RfdetrNano | Profile::RfdetrCoco => {
                Inference::Rfdetr(Box::new(RFDETR::new(config)?))
            }
            Profile::PicodetLCoco | Profile::PicodetCoco => {
                Inference::Picodet(Box::new(PicoDet::new(config)?))
            }
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
            threshold,
            cap,
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture(infer: fn(&Image) -> Result<Vec<Detection>>) -> Self {
        Self {
            runtime: Inference::Fixture(infer),
            threshold: 0.5,
            cap: 100,
        }
    }

    pub fn run(&mut self, image: &Image) -> Result<Vec<Detection>> {
        match &mut self.runtime {
            Inference::Rfdetr(runtime) => {
                // Upstream clips the origin before preserving width/height, shifting edge boxes.
                let (model, engines) = runtime.parts_mut();
                ensure!(
                    matches!(
                        model.processor.resize_mode_type(),
                        Some(ResizeModeType::Letterbox)
                    ),
                    "RF-DETR requires the Letterbox preset"
                );
                let inputs = model.processor.process(std::slice::from_ref(image))?;
                let outputs = engines.run(&usls::Module::Model, &inputs)?.into_inner();
                let boxes = outputs
                    .get("pred_boxes")
                    .or_else(|| outputs.get("boxes"))
                    .context("missing RF-DETR boxes")?
                    .try_extract_array::<f32>()
                    .context("RF-DETR boxes must be FP32 CPU-accessible tensors")?;
                let logits = outputs
                    .get("pred_logits")
                    .or_else(|| outputs.get("logits"))
                    .context("missing RF-DETR logits")?
                    .try_extract_array::<f32>()
                    .context("RF-DETR logits must be FP32 CPU-accessible tensors")?;
                ensure!(
                    model.processor.images_transform_info.len() == 1,
                    "unexpected RF-DETR transform count"
                );
                decode_rfdetr(
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
                )
            }
            Inference::Rtdetr(runtime) => run_rtdetr(runtime, image, self.threshold, self.cap),
            Inference::Picodet(runtime) => run_picodet(runtime, image, self.threshold, self.cap),
            #[cfg(test)]
            Inference::Fixture(infer) => sanitize(
                infer(image)?,
                image.width(),
                image.height(),
                self.threshold,
                self.cap,
            ),
        }
    }
}

fn run_rtdetr(
    runtime: &mut Runtime<RTDETR>,
    image: &Image,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    let (model, engines) = runtime.parts_mut();
    ensure!(
        matches!(
            model.processor.resize_mode_type(),
            Some(ResizeModeType::Letterbox)
        ),
        "RT-DETR requires the Letterbox preset"
    );
    let input = model.processor.process(std::slice::from_ref(image))?;
    let sizes = X::from(vec![
        f32::from(u16::try_from(model.height).context("model height exceeds bound")?),
        f32::from(u16::try_from(model.width).context("model width exceeds bound")?),
    ])
    .insert_axis(0)?;
    let outputs = engines
        .run(&usls::Module::Model, usls::inputs![&input, sizes]?)?
        .into_inner();
    let labels = outputs
        .get("labels")
        .context("missing labels")?
        .try_extract_array::<i64>()?;
    let boxes = outputs
        .get("boxes")
        .context("missing boxes")?
        .try_extract_array::<f32>()?;
    let scores = outputs
        .get("scores")
        .context("missing scores")?
        .try_extract_array::<f32>()?;
    ensure!(
        model.processor.images_transform_info.len() == 1,
        "unexpected transform count"
    );
    decode_rtdetr(
        (
            labels.shape(),
            labels.as_slice().context("noncontiguous labels")?,
        ),
        (
            boxes.shape(),
            boxes.as_slice().context("noncontiguous boxes")?,
        ),
        (
            scores.shape(),
            scores.as_slice().context("noncontiguous scores")?,
        ),
        model
            .processor
            .images_transform_info
            .first()
            .context("missing transform")?,
        threshold,
        cap,
    )
}

fn run_picodet(
    runtime: &mut Runtime<PicoDet>,
    image: &Image,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    let (model, engines) = runtime.parts_mut();
    let input = model.processor.process(std::slice::from_ref(image))?;
    ensure!(
        model.processor.images_transform_info.len() == 1,
        "unexpected transform count"
    );
    let info = model
        .processor
        .images_transform_info
        .first()
        .context("missing transform")?;
    let scales = X::from(vec![info.height_scale, info.width_scale]).insert_axis(0)?;
    let outputs = engines
        .run(&usls::Module::Model, usls::inputs![&input, scales]?)?
        .into_inner();
    let rows = outputs
        .get("multiclass_nms3_0.tmp_0")
        .context("missing PicoDet detections")?
        .try_extract_array::<f32>()?;
    let count = outputs
        .get("multiclass_nms3_0.tmp_2")
        .context("missing PicoDet count")?
        .try_extract_array::<i32>()?;
    decode_picodet(
        (
            rows.shape(),
            rows.as_slice().context("noncontiguous detections")?,
        ),
        (
            count.shape(),
            count.as_slice().context("noncontiguous count")?,
        ),
        [image.width(), image.height()],
        threshold,
        cap,
    )
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

fn decode_rtdetr(
    (label_shape, labels): (&[usize], &[i64]),
    (box_shape, boxes): (&[usize], &[f32]),
    (score_shape, scores): (&[usize], &[f32]),
    info: &ImageTransformInfo,
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    ensure!(
        label_shape == [1, labels.len()]
            && score_shape == [1, labels.len()]
            && box_shape == [1, labels.len(), 4]
            && scores.len() == labels.len()
            && labels.len().checked_mul(4) == Some(boxes.len()),
        "RT-DETR output shapes disagree: {label_shape:?}, {box_shape:?}, {score_shape:?}"
    );
    ensure!(
        info.height_scale.is_finite()
            && info.height_scale > 0.0
            && info.width_pad.is_finite()
            && info.height_pad.is_finite(),
        "invalid RT-DETR letterbox geometry"
    );
    let mut detections = Vec::new();
    for ((&label, &score), &[x1, y1, x2, y2]) in
        labels.iter().zip(scores).zip(boxes.as_chunks::<4>().0)
    {
        let label = class_label(
            Profile::RtdetrV1Coco,
            usize::try_from(label).context("negative class")?,
        )?;
        ensure!(
            [x1, y1, x2, y2].iter().all(|value| value.is_finite()),
            "nonfinite detection"
        );
        detections.push(Detection {
            label: glib::Quark::from_str(label),
            score,
            xyxy: [
                (x1 - info.width_pad) / info.height_scale,
                (y1 - info.height_pad) / info.height_scale,
                (x2 - info.width_pad) / info.height_scale,
                (y2 - info.height_pad) / info.height_scale,
            ]
            .map(f64::from),
        });
    }
    sanitize(detections, info.width_src, info.height_src, threshold, cap)
}

fn decode_picodet(
    (shape, rows): (&[usize], &[f32]),
    (count_shape, counts): (&[usize], &[i32]),
    [width, height]: [u32; 2],
    threshold: f32,
    cap: usize,
) -> Result<Vec<Detection>> {
    let count = match counts {
        [count] => usize::try_from(*count).context("negative PicoDet count")?,
        _ => anyhow::bail!("unexpected PicoDet count"),
    };
    ensure!(
        count_shape == [1] && shape == [count, 6] && count.checked_mul(6) == Some(rows.len()),
        "PicoDet output shapes disagree: {shape:?}, {count_shape:?}, count={count}"
    );
    let mut detections = Vec::new();
    for &[class, score, x1, y1, x2, y2] in rows.as_chunks::<6>().0 {
        // Comparing to the small integer domain also rejects fractional and nonfinite IDs.
        #[expect(
            clippy::float_cmp,
            reason = "class IDs must equal an exactly representable integer"
        )]
        let label = (0_u8..80)
            .find(|id| f32::from(*id) == class)
            .context("invalid PicoDet COCO80 class")?;
        detections.push(Detection {
            label: glib::Quark::from_str(class_label(Profile::PicodetCoco, usize::from(label))?),
            score,
            xyxy: [x1, y1, x2, y2].map(f64::from),
        });
    }
    sanitize(detections, width, height, threshold, cap)
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
    let index = if profile.is_rfdetr() {
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

    const PROFILES: [Profile; 17] = [
        Profile::RfdetrNano,
        Profile::RtdetrV1R18,
        Profile::RtdetrV2S,
        Profile::RtdetrV4S,
        Profile::PicodetLCoco,
        Profile::DfineNCoco,
        Profile::DeimDfineSCoco,
        Profile::Deimv2AttoCoco,
        Profile::RfdetrCoco,
        Profile::RtdetrV1Coco,
        Profile::RtdetrV2Coco,
        Profile::RtdetrV4Coco,
        Profile::PicodetCoco,
        Profile::DfineCoco,
        Profile::DeimDfineCoco,
        Profile::Deimv2Hgnetv2Coco,
        Profile::Deimv2Dinov3Coco,
    ];

    #[test]
    fn labels_cover_dense_and_sparse_coco() -> Result<()> {
        for profile in PROFILES {
            let ids = if profile.is_rfdetr() {
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
            if !profile.is_rfdetr() {
                class_label(profile, 80).expect_err("dense COCO80 upper bound");
            }
        }
        Ok(())
    }

    #[test]
    fn profiles_reject_unqualified_units() -> Result<()> {
        for profile in PROFILES {
            verify_units(profile, Provider::Coreml, ComputeUnits::CpuAndGpu)?;
            for units in [ComputeUnits::All, ComputeUnits::CpuAndNeuralEngine] {
                verify_units(profile, Provider::Cpu, units)?;
                if profile == Profile::RfdetrNano {
                    verify_units(profile, Provider::Coreml, units)?;
                } else {
                    verify_units(profile, Provider::Coreml, units).expect_err("unqualified units");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn model_file_must_exist_locally() -> Result<()> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        verify_model(directory).expect_err("directory is not a model");
        verify_model(&directory.join("missing.onnx")).expect_err("no implicit downloads");
        verify_model(&directory.join("Cargo.toml"))?; // ORT validates the contents at startup.
        Ok(())
    }

    #[test]
    fn aliases_preserve_preprocessing_and_dinov3_normalizes() -> Result<()> {
        for (alias, family) in [
            (Profile::RfdetrNano, Profile::RfdetrCoco),
            (Profile::RtdetrV1R18, Profile::RtdetrV1Coco),
            (Profile::RtdetrV2S, Profile::RtdetrV2Coco),
            (Profile::RtdetrV4S, Profile::RtdetrV4Coco),
            (Profile::PicodetLCoco, Profile::PicodetCoco),
            (Profile::DfineNCoco, Profile::DfineCoco),
            (Profile::DeimDfineSCoco, Profile::DeimDfineCoco),
            (Profile::Deimv2AttoCoco, Profile::Deimv2Hgnetv2Coco),
        ] {
            ensure!(
                alias.config().image_processor() == family.config().image_processor(),
                "alias preprocessing"
            );
            ensure!(
                alias.is_rfdetr() == family.is_rfdetr()
                    && alias.is_picodet() == family.is_picodet(),
                "alias decoder"
            );
        }
        let hgnet = Profile::Deimv2Hgnetv2Coco.config();
        let dino = Profile::Deimv2Dinov3Coco.config();
        ensure!(
            hgnet.image_processor().image_mean.is_none()
                && hgnet.image_processor().image_std.is_none(),
            "HGNetv2 only scales pixels"
        );
        ensure!(
            dino.image_processor()
                .image_mean
                .map(|v| v.map(f32::to_bits))
                == Some([0.485_f32, 0.456, 0.406].map(f32::to_bits)),
            "DINOv3 mean"
        );
        ensure!(
            dino.image_processor()
                .image_std
                .map(|v| v.map(f32::to_bits))
                == Some([0.229_f32, 0.224, 0.225].map(f32::to_bits)),
            "DINOv3 std"
        );
        Ok(())
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

    fn contract(profile: Profile, size: usize, queries: usize) -> (OrtTensorAttr, OrtTensorAttr) {
        use TensorElementType::{Float32, Int32, Int64};
        if profile.is_rfdetr() {
            (
                attrs(&["images"], vec![vec![1, 3, size, size]], vec![Float32]),
                attrs(
                    &["pred_boxes", "pred_logits"],
                    vec![vec![1, queries, 4], vec![1, queries, 91]],
                    vec![Float32, Float32],
                ),
            )
        } else if profile.is_picodet() {
            (
                attrs(
                    &["image", "scale_factor"],
                    vec![vec![1, 3, size, size], vec![1, 2]],
                    vec![Float32, Float32],
                ),
                attrs(
                    &["multiclass_nms3_0.tmp_0", "multiclass_nms3_0.tmp_2"],
                    vec![vec![queries, 6], vec![1]],
                    vec![Float32, Int32],
                ),
            )
        } else {
            (
                attrs(
                    &["images", "orig_target_sizes"],
                    vec![vec![1, 3, size, size], vec![1, 2]],
                    vec![Float32, Int64],
                ),
                attrs(
                    &["labels", "boxes", "scores"],
                    vec![vec![1, queries], vec![1, queries, 4], vec![1, queries]],
                    vec![Int64, Float32, Float32],
                ),
            )
        }
    }

    #[test]
    fn tensor_contracts_accept_sizes_and_symbolic_outputs() -> Result<()> {
        for profile in PROFILES {
            for size in [320, 384, 640, 704] {
                for queries in [0, 100, 300, 500] {
                    let (inputs, mut outputs) = contract(profile, size, queries);
                    verify_contract(profile, &inputs, &outputs)?;
                    for shape in &mut outputs.dimss {
                        shape.fill(0);
                    }
                    verify_contract(profile, &inputs, &outputs)?;
                }
            }
        }
        let (inputs, mut outputs) = contract(Profile::RfdetrCoco, 704, 300);
        outputs.names = vec!["boxes".into(), "logits".into()];
        verify_contract(Profile::RfdetrCoco, &inputs, &outputs)?;
        Ok(())
    }

    #[test]
    fn tensor_contracts_reject_incompatible_graphs() -> Result<()> {
        for profile in PROFILES {
            let (inputs, outputs) = contract(profile, 640, 300);
            for shape in [
                vec![1, 3, 640],
                vec![0, 3, 640, 640],
                vec![2, 3, 640, 640],
                vec![1, 4, 640, 640],
                vec![1, 3, 0, 0],
                vec![1, 3, 320, 640],
                vec![1, 3, 65536, 65536],
            ] {
                let mut wrong = inputs.clone();
                *wrong.dimss.first_mut().context("image input")? = shape;
                verify_contract(profile, &wrong, &outputs).expect_err("invalid image shape");
            }
            for output in [false, true] {
                let mut wrong = if output {
                    outputs.clone()
                } else {
                    inputs.clone()
                };
                *wrong.names.first_mut().context("tensor name")? = "wrong".into();
                if output {
                    verify_contract(profile, &inputs, &wrong)
                } else {
                    verify_contract(profile, &wrong, &outputs)
                }
                .expect_err("invalid names");
                let mut wrong = if output {
                    outputs.clone()
                } else {
                    inputs.clone()
                };
                *wrong.dtypes.first_mut().context("dtype")? = TensorElementType::Float16;
                if output {
                    verify_contract(profile, &inputs, &wrong)
                } else {
                    verify_contract(profile, &wrong, &outputs)
                }
                .expect_err("invalid precision");
            }
            let mut wrong = outputs.clone();
            wrong.names.push("masks".into());
            wrong.dimss.push(vec![1, 300, 640, 640]);
            wrong.dtypes.push(TensorElementType::Float32);
            verify_contract(profile, &inputs, &wrong).expect_err("segmentation is unsupported");
            let mut wrong = outputs.clone();
            wrong.dimss.first_mut().context("output shape")?.pop();
            verify_contract(profile, &inputs, &wrong).expect_err("invalid output rank");
            let mut wrong = outputs.clone();
            *wrong
                .dimss
                .get_mut(1)
                .context("second output")?
                .last_mut()
                .context("dimension")? = 90;
            verify_contract(profile, &inputs, &wrong)
                .expect_err("invalid class, box or count dimension");
            if !profile.is_picodet() {
                let mut wrong = outputs.clone();
                *wrong
                    .dimss
                    .first_mut()
                    .context("output")?
                    .first_mut()
                    .context("batch")? = 2;
                verify_contract(profile, &inputs, &wrong).expect_err("invalid output batch");
                let mut wrong = outputs.clone();
                *wrong
                    .dimss
                    .first_mut()
                    .context("output")?
                    .get_mut(1)
                    .context("queries")? = 299;
                verify_contract(profile, &inputs, &wrong).expect_err("query counts disagree");
            }
            if !profile.is_rfdetr() {
                let mut wrong = inputs.clone();
                wrong.names.swap(0, 1);
                verify_contract(profile, &wrong, &outputs)
                    .expect_err("positional inputs reordered");
                let mut wrong = inputs.clone();
                *wrong.dimss.get_mut(1).context("auxiliary input")? = vec![0, 2];
                verify_contract(profile, &wrong, &outputs).expect_err("dynamic auxiliary batch");
            }
        }
        Ok(())
    }

    #[test]
    fn checked_dense_decoders_reject_invalid_rows_and_preserve_geometry() -> Result<()> {
        let info = ImageTransformInfo {
            width_src: 400,
            height_src: 200,
            height_scale: 0.5,
            height_pad: 50.0,
            ..ImageTransformInfo::default()
        };
        let boxes = [-5.0, 60.0, 15.0, 80.0];
        let run = |labels: &[i64], boxes: &[f32], scores: &[f32]| {
            decode_rtdetr(
                (&[1, labels.len()], labels),
                (&[1, 1, 4], boxes),
                (&[1, scores.len()], scores),
                &info,
                0.5,
                100,
            )
        };
        let detections = run(&[73], &boxes, &[0.9])?;
        let bbox = detections.first().context("detection")?;
        ensure!(
            bbox.label.as_str() == "book"
                && bbox.xyxy.map(f64::to_bits) == [0.0_f64, 20.0, 30.0, 60.0].map(f64::to_bits),
            "independent endpoint clipping"
        );
        for labels in [&[-1_i64][..], &[80][..], &[][..], &[0, 1][..]] {
            run(labels, &boxes, &[0.9]).expect_err("invalid labels/count");
        }
        for score in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            run(&[0], &boxes, &[score]).expect_err("invalid confidence, including below threshold");
        }
        run(&[0], &[f32::NAN; 4], &[0.1]).expect_err("nonfinite low-score box");
        run(&[0], &boxes, &[]).expect_err("no silent truncation");
        let run = |rows: &[f32], count: i32| {
            decode_picodet((&[1, 6], rows), (&[1], &[count]), [400, 200], 0.5, 100)
        };
        let detections = run(&[73.0, 0.9, -10.0, 20.0, 30.0, 60.0], 1)?;
        ensure!(
            detections
                .first()
                .context("PicoDet result")?
                .xyxy
                .map(f64::to_bits)
                == bbox.xyxy.map(f64::to_bits),
            "PicoDet coordinates are already in source space"
        );
        ensure!(
            run(&[-0.0, 0.9, 0.0, 0.0, 1.0, 1.0], 1)?
                .first()
                .context("zero class")?
                .label
                .as_str()
                == "person",
            "signed zero is class zero"
        );
        for class in [-1.0, 0.5, 80.0, f32::NAN, f32::INFINITY] {
            run(&[class, 0.1, 0.0, 0.0, 1.0, 1.0], 1)
                .expect_err("invalid class, including below threshold");
        }
        for count in [-1, 0, 2] {
            run(&[0.0; 6], count).expect_err("invalid count");
        }
        run(&[0.0; 5], 1).expect_err("incomplete row");
        ensure!(
            decode_rtdetr(
                (&[1, 0], &[]),
                (&[1, 0, 4], &[]),
                (&[1, 0], &[]),
                &info,
                0.5,
                100
            )?
            .is_empty(),
            "empty RT-DETR"
        );
        ensure!(
            decode_picodet((&[0, 6], &[]), (&[1], &[0]), [400, 200], 0.5, 100)?.is_empty(),
            "empty PicoDet"
        );
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
