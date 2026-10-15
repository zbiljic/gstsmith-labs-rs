use std::{fs::File, path::Path};

use anyhow::{Context, Result, ensure};
use gst::glib;
use ort::value::TensorElementType;
use usls::{
    Config, Device, Engine, FromConfig, Image, ImageProcessor, Module, OrtTensorAttr,
    ResizeModeType, SamKind, X,
};

use crate::backend::{Capture, ComputeUnits, Provider, logger_owned, verify_assignment};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstUslsSegmentationProfile")]
pub(crate) enum Profile {
    #[default]
    #[enum_value(name = "FastSAM", nick = "fastsam")]
    FastSam,
    #[enum_value(name = "YOLOE v8 prompt-free", nick = "yoloe-v8-pf")]
    YoloE8,
    #[enum_value(name = "YOLOE 11 prompt-free", nick = "yoloe-11-pf")]
    YoloE11,
    #[enum_value(name = "YOLOE 26 prompt-free", nick = "yoloe-26-pf")]
    YoloE26,
    #[enum_value(name = "EdgeCrafter COCO", nick = "ecdetseg-coco")]
    EcDetSeg,
    #[enum_value(name = "BiRefNet COD", nick = "birefnet-cod")]
    BiRefCod,
    #[enum_value(name = "BiRefNet DIS", nick = "birefnet-dis")]
    BiRefDis,
    #[enum_value(name = "BiRefNet HRSOD", nick = "birefnet-hrsod")]
    BiRefHrsod,
    #[enum_value(name = "BiRefNet Massive", nick = "birefnet-massive")]
    BiRefMassive,
    #[enum_value(name = "SAM", nick = "sam")]
    Sam,
    #[enum_value(name = "SAM-HQ", nick = "sam-hq")]
    SamHq,
    #[enum_value(name = "MobileSAM", nick = "mobile-sam")]
    MobileSam,
    #[enum_value(name = "EdgeSAM", nick = "edge-sam")]
    EdgeSam,
    #[enum_value(name = "SAM 2.0", nick = "sam2")]
    Sam2,
    #[enum_value(name = "SAM 2.1", nick = "sam2.1")]
    Sam21,
    #[enum_value(name = "SAM3 Tracker (per-frame)", nick = "sam3-tracker")]
    Sam3,
}

impl Profile {
    pub fn prompted(self) -> bool {
        matches!(
            self,
            Self::Sam
                | Self::SamHq
                | Self::MobileSam
                | Self::EdgeSam
                | Self::Sam2
                | Self::Sam21
                | Self::Sam3
        )
    }
    pub fn semantic(self) -> bool {
        matches!(
            self,
            Self::BiRefCod | Self::BiRefDis | Self::BiRefHrsod | Self::BiRefMassive
        )
    }
    fn config(self) -> Config {
        match self {
            Self::FastSam => Config::fastsam(),
            Self::YoloE8 => Config::yoloe_seg_pf().with_version(8.into()),
            Self::YoloE11 => Config::yoloe_seg_pf().with_version(11.into()),
            Self::YoloE26 => Config::yoloe_seg_pf().with_version(26.into()),
            Self::EcDetSeg => Config::ecdetseg(),
            Self::BiRefCod | Self::BiRefDis | Self::BiRefHrsod | Self::BiRefMassive => {
                Config::birefnet()
            }
            Self::Sam | Self::Sam21 => Config::sam(),
            Self::SamHq => Config::sam().with_sam_kind(SamKind::SamHq),
            Self::MobileSam => Config::sam().with_sam_kind(SamKind::MobileSam),
            Self::EdgeSam => Config::sam().with_sam_kind(SamKind::EdgeSam),
            Self::Sam2 => Config::sam().with_sam_kind(SamKind::Sam2),
            Self::Sam3 => Config::sam3_tracker(),
        }
    }
    fn classes(self) -> &'static [&'static str] {
        match self {
            Self::FastSam => &["object"],
            Self::EcDetSeg => &usls::NAMES_COCO_80,
            Self::YoloE8 | Self::YoloE11 | Self::YoloE26 => &usls::NAMES_YOLOE_4585,
            _ => &["foreground"],
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Prompt {
    pub bbox: Option<[f32; 4]>, // source xyxy
    pub points: Vec<([f32; 2], bool)>,
    pub source: Option<u32>,
    pub label: glib::Quark,
}

pub(crate) fn parse_prompt(text: &str) -> Result<Prompt> {
    ensure!(text.len() <= 8192, "prompt exceeds 8192 bytes");
    let mut prompt = Prompt {
        bbox: None,
        points: vec![],
        source: None,
        label: glib::Quark::from_str("object"),
    };
    for item in text.split(';') {
        let (kind, values) = item
            .trim()
            .split_once(':')
            .context("prompt requires box:x,y,w,h;point:x,y;exclude:x,y")?;
        let values = values
            .split(',')
            .map(|s| s.trim().parse::<f32>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            values.iter().all(|v| v.is_finite() && *v >= 0.0),
            "prompt coordinates must be finite and nonnegative"
        );
        match (kind, values.as_slice()) {
            ("box", &[x, y, w, h]) => {
                ensure!(
                    prompt.bbox.is_none()
                        && w > 0.0
                        && h > 0.0
                        && (x + w).is_finite()
                        && (y + h).is_finite(),
                    "one positive nonempty box is permitted"
                );
                prompt.bbox = Some([x, y, x + w, y + h]);
            }
            ("point" | "exclude", &[x, y]) => {
                ensure!(prompt.points.len() < 64, "at most 64 prompt points");
                prompt.points.push(([x, y], kind == "point"));
            }
            _ => anyhow::bail!("invalid prompt item: {item}"),
        }
    }
    ensure!(
        prompt.bbox.is_some() || prompt.points.iter().any(|(_, positive)| *positive),
        "prompt needs a box or positive point"
    );
    Ok(prompt)
}

#[derive(Debug)]
pub(crate) struct Region {
    pub pixels: Vec<u8>,
    pub label: glib::Quark,
    pub score: f32,
    pub source: Option<u32>,
    pub bbox: Option<[f64; 4]>,
}

#[derive(Clone)]
pub(crate) struct Options {
    pub profile: Profile,
    pub threshold: f32,
    pub mask_threshold: f32,
    pub cap: usize,
    pub maximum: usize,
    pub provider: Provider,
    pub units: ComputeUnits,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            profile: Profile::FastSam,
            threshold: 0.5,
            mask_threshold: 0.5,
            cap: 16,
            maximum: 64 * 1024 * 1024,
            provider: Provider::Cpu,
            units: ComputeUnits::CpuAndGpu,
        }
    }
}

pub(crate) struct Runtime {
    engine: Engine,
    decoder: Option<Engine>,
    processor: ImageProcessor,
    size: [usize; 2],
    options: Options,
}

fn local_file(path: &Path) -> Result<String> {
    let path = path.canonicalize().context("resolving local model file")?;
    ensure!(
        File::open(&path)?.metadata()?.is_file(),
        "model must be a readable regular file"
    );
    Ok(path
        .to_str()
        .context("model path must be UTF-8")?
        .to_owned())
}

impl Runtime {
    #[expect(
        clippy::too_many_lines,
        reason = "validate local assets and build the two related sessions together"
    )]
    pub fn new(
        model: Option<&Path>,
        encoder: Option<&Path>,
        decoder: Option<&Path>,
        options: Options,
    ) -> Result<Self> {
        ensure!((1..=1000).contains(&options.cap), "invalid max-regions");
        ensure!(
            (1..=512 * 1024 * 1024).contains(&options.maximum),
            "invalid max-mask-bytes"
        );
        ensure!(
            [options.threshold, options.mask_threshold]
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
            "thresholds must be finite in 0..1"
        );
        ensure!(
            options.provider != Provider::Coreml
                || cfg!(all(
                    feature = "coreml",
                    target_os = "macos",
                    target_arch = "aarch64"
                )),
            "CoreML requires the coreml feature on Apple Silicon macOS"
        );
        ensure!(
            options.provider != Provider::Coreml || options.units == ComputeUnits::CpuAndGpu,
            "segmentation CoreML supports only cpu-and-gpu"
        );
        // shortcut: prompted decoders have dynamic point/box dimensions; qualify CoreML exports before enabling it.
        ensure!(
            options.provider != Provider::Coreml || !options.profile.prompted(),
            "prompted segmentation currently requires CPU; CoreML decoder shapes are unqualified"
        );
        let mut config = options.profile.config();
        let module = if options.profile == Profile::Sam3 {
            Module::VisualEncoder
        } else if options.profile.prompted() {
            Module::Encoder
        } else {
            Module::Model
        };
        if options.profile.prompted() {
            ensure!(
                model.is_none(),
                "prompted profiles require encoder-file and decoder-file, not model-file"
            );
            let enc = local_file(encoder.context("encoder-file is required")?)?;
            let dec = local_file(decoder.context("decoder-file is required")?)?;
            config = if options.profile == Profile::Sam3 {
                config.with_visual_encoder_file(enc)
            } else {
                config.with_encoder_file(enc)
            };
            config = config.with_decoder_file(dec);
        } else {
            ensure!(
                encoder.is_none() && decoder.is_none(),
                "automatic profiles use model-file only"
            );
            config = config.with_model_file(local_file(model.context("model-file is required")?)?);
        }
        config = config
            .with_batch_size_min_opt_max_all(1, 1, 1)
            .with_num_dry_run_all(0)
            .with_num_intra_threads_all(10)
            .with_num_inter_threads_all(1)
            .with_device_all(if options.provider == Provider::Cpu {
                Device::Cpu(0)
            } else {
                Device::CoreMl
            })
            .with_coreml_static_input_shapes_all(true)
            .with_coreml_subgraph_running_all(true)
            .with_coreml_model_format_all(0)
            .with_coreml_specialization_strategy_all(1)
            .with_coreml_compute_units_all(options.units as u8)
            .commit()?;
        let owned = logger_owned()?;
        ensure!(
            options.provider != Provider::Coreml || owned,
            "CoreML assignment requires the usls ORT logger"
        );
        let capture = Capture::begin()?;
        let engine = Engine::from_config(config.take_module(&module)?)?;
        verify_assignment(options.provider, &capture.finish())?;
        let size = image_contract(&engine.inputs, options.profile == Profile::EcDetSeg)?;
        let processor = ImageProcessor::from_config(config.image_processor().clone())?
            .with_image_width(u32::try_from(size[0])?)
            .with_image_height(u32::try_from(size[1])?);
        let decoder = if options.profile.prompted() {
            let decoder = Engine::from_config(config.take_module(&Module::Decoder)?)?;
            prompt_contract(
                options.profile,
                &engine.outputs,
                &decoder.inputs,
                &decoder.outputs,
            )?;
            Some(decoder)
        } else {
            automatic_contract(options.profile, &engine.outputs)?;
            None
        };
        Ok(Self {
            engine,
            decoder,
            processor,
            size,
            options,
        })
    }

    pub fn run(&mut self, image: &Image, prompts: &[Prompt]) -> Result<Vec<Region>> {
        let pixels = usize::try_from(image.width())?
            .checked_mul(usize::try_from(image.height())?)
            .context("mask size overflow")?;
        ensure!(
            pixels <= self.options.maximum,
            "one mask exceeds max-mask-bytes"
        );
        if self.options.profile.prompted() && prompts.is_empty() {
            return Ok(vec![]);
        }
        let input = self.processor.process(std::slice::from_ref(image))?;
        let info = self
            .processor
            .images_transform_info
            .first()
            .context("missing image transform")?;
        let mapping = Mapping::new(
            image.width(),
            image.height(),
            self.size,
            self.processor
                .resize_mode_type()
                .context("missing resize mode")?,
            info,
        )?;
        let outputs = if self.options.profile == Profile::EcDetSeg {
            let sizes = X::from_shape_vec(
                &[1, 2],
                vec![
                    f32::from(u16::try_from(self.size[1])?),
                    f32::from(u16::try_from(self.size[0])?),
                ],
            )?;
            let attrs = self.engine.outputs.clone();
            collect(self.engine.run(usls::inputs![&input, sizes]?)?, &attrs)?
        } else {
            let attrs = self.engine.outputs.clone();
            collect(self.engine.run(&input)?, &attrs)?
        };
        if let Some(decoder) = self.decoder.as_mut() {
            let mut regions = Vec::new();
            let cap = self.options.cap.min(self.options.maximum / pixels);
            for prompt in prompts.iter().take(cap) {
                if let Some(region) = run_prompt(
                    decoder,
                    self.options.profile,
                    &outputs,
                    prompt,
                    &mapping,
                    &self.options,
                )? {
                    regions.push(region);
                }
            }
            Ok(regions)
        } else {
            decode_automatic(&outputs, &mapping, &self.options)
        }
    }
}

fn image_contract(inputs: &OrtTensorAttr, auxiliary: bool) -> Result<[usize; 2]> {
    ensure!(
        inputs.dimss.len() == if auxiliary { 2 } else { 1 },
        "unexpected image inputs"
    );
    let Some([1, 3, h, w]) = inputs.dimss.first().map(Vec::as_slice) else {
        anyhow::bail!("expected static batch-one NCHW RGB input");
    };
    ensure!(
        *h > 0
            && *w > 0
            && *h <= 65535
            && *w <= 65535
            && h.checked_mul(*w)
                .and_then(|n| n.checked_mul(12))
                .is_some_and(|n| n <= 128 * 1024 * 1024),
        "invalid or oversized image tensor"
    );
    ensure!(
        inputs.dtypes.first() == Some(&TensorElementType::Float32),
        "image input must be FP32"
    );
    if auxiliary {
        ensure!(
            inputs.dimss.get(1).is_some_and(|v| v == &[1, 2])
                && inputs.dtypes.get(1) == Some(&TensorElementType::Int64),
            "ECDetSeg requires INT64 target sizes [1,2]"
        );
    }
    Ok([*w, *h])
}

fn automatic_contract(profile: Profile, outputs: &OrtTensorAttr) -> Result<()> {
    let ranks: &[usize] = if profile.semantic() {
        &[4]
    } else if profile == Profile::EcDetSeg {
        &[2, 3, 2, 4]
    } else {
        &[3, 4]
    };
    ensure!(
        outputs.dimss.len() == ranks.len() && outputs.dtypes.len() == ranks.len(),
        "wrong segmentation output count"
    );
    for (i, (shape, rank)) in outputs.dimss.iter().zip(ranks).enumerate() {
        ensure!(
            shape.len() == *rank && shape.first().is_some_and(|b| *b <= 1),
            "invalid segmentation output shape"
        );
        let dtype = if profile == Profile::EcDetSeg && i == 0 {
            TensorElementType::Int64
        } else {
            TensorElementType::Float32
        };
        ensure!(
            outputs.dtypes.get(i) == Some(&dtype),
            "invalid segmentation output dtype"
        );
    }
    Ok(())
}

fn prompt_contract(
    profile: Profile,
    encoder: &OrtTensorAttr,
    inputs: &OrtTensorAttr,
    outputs: &OrtTensorAttr,
) -> Result<()> {
    let (features, arguments, results) = match profile {
        Profile::Sam | Profile::MobileSam => (1, 6, 3),
        Profile::SamHq => (2, 7, 3),
        Profile::EdgeSam => (1, 3, 2),
        Profile::Sam2 | Profile::Sam21 => (3, 8, 3),
        Profile::Sam3 => (3, 6, 3),
        _ => anyhow::bail!("not a prompted profile"),
    };
    ensure!(
        encoder.dimss.len() == features
            && inputs.dimss.len() == arguments
            && (outputs.dimss.len() == results
                || (matches!(profile, Profile::Sam2 | Profile::Sam21) && outputs.dimss.len() == 2)),
        "prompted encoder/decoder contract mismatch"
    );
    ensure!(
        encoder
            .dtypes
            .iter()
            .chain(outputs.dtypes.iter())
            .all(|t| *t == TensorElementType::Float32),
        "prompted outputs must be FP32"
    );
    ensure!(
        inputs
            .dtypes
            .iter()
            .all(|t| matches!(t, TensorElementType::Float32 | TensorElementType::Int64)),
        "unsupported decoder input dtype"
    );
    ensure!(
        encoder.dimss.iter().all(|s| s.len() >= 4 && s.len() <= 5),
        "invalid embedding rank"
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct Tensor {
    shape: Vec<usize>,
    data: Vec<f32>,
}
impl Tensor {
    fn x(&self) -> Result<X> {
        X::from_shape_vec(&self.shape, self.data.clone())
    }
}

fn collect(outputs: usls::Xs<'_>, attrs: &OrtTensorAttr) -> Result<Vec<Tensor>> {
    let outputs = outputs.into_inner();
    ensure!(
        outputs.len() == attrs.names.len(),
        "inference output count changed"
    );
    let mut result = Vec::new();
    let mut bytes = 0usize;
    for (index, name) in attrs.names.iter().enumerate() {
        let output = outputs.get(name).context("missing output tensor")?;
        let (shape, data) = if attrs.dtypes.get(index) == Some(&TensorElementType::Int64) {
            let array = output.try_extract_array::<i64>()?;
            bytes = bytes
                .checked_add(array.len().checked_mul(8).context("tensor overflow")?)
                .context("tensor overflow")?;
            ensure!(bytes <= 256 * 1024 * 1024, "output tensors exceed 256 MiB");
            let data = array
                .iter()
                .map(|&id| {
                    ensure!((0..=4585).contains(&id), "invalid output class id");
                    Ok(f32::from(u16::try_from(id)?))
                })
                .collect::<Result<Vec<_>>>()?;
            (array.shape().to_vec(), data)
        } else {
            let array = output.try_extract_array::<f32>()?;
            bytes = bytes
                .checked_add(array.len().checked_mul(4).context("tensor overflow")?)
                .context("tensor overflow")?;
            ensure!(bytes <= 256 * 1024 * 1024, "output tensors exceed 256 MiB");
            ensure!(
                array.iter().all(|v| v.is_finite()),
                "nonfinite segmentation tensor"
            );
            (array.shape().to_vec(), array.iter().copied().collect())
        };
        let declared = attrs
            .dimss
            .get(index)
            .context("missing declared output shape")?;
        ensure!(
            shape.len() == declared.len()
                && shape.iter().zip(declared).all(|(a, b)| *b == 0 || a == b),
            "runtime output shape differs from declaration"
        );
        result.push(Tensor { shape, data });
    }
    Ok(result)
}

#[derive(Clone, Debug)]
struct Mapping {
    width: u32,
    height: u32,
    model: [usize; 2],
    sx: f64,
    sy: f64,
    px: f64,
    py: f64,
}
impl Mapping {
    fn new(
        width: u32,
        height: u32,
        model: [usize; 2],
        mode: ResizeModeType,
        info: &usls::ImageTransformInfo,
    ) -> Result<Self> {
        let (sx, sy, px, py) = match mode {
            ResizeModeType::FitExact => (
                f64::from(u16::try_from(model[0])?) / f64::from(width),
                f64::from(u16::try_from(model[1])?) / f64::from(height),
                0.0,
                0.0,
            ),
            ResizeModeType::FitAdaptive | ResizeModeType::Letterbox => (
                f64::from(info.width_scale),
                f64::from(info.height_scale),
                f64::from(info.width_pad),
                f64::from(info.height_pad),
            ),
            _ => anyhow::bail!("unsupported segmentation resize mode"),
        };
        ensure!(
            [sx, sy, px, py].iter().all(|v| v.is_finite()) && sx > 0.0 && sy > 0.0,
            "invalid resize transform"
        );
        Ok(Self {
            width,
            height,
            model,
            sx,
            sy,
            px,
            py,
        })
    }
    fn bbox(&self, [x1, y1, x2, y2]: [f32; 4]) -> Result<[f64; 4]> {
        ensure!(
            [x1, y1, x2, y2].iter().all(|v| v.is_finite()) && x2 >= x1 && y2 >= y1,
            "invalid segmentation box"
        );
        Ok([
            ((f64::from(x1) - self.px) / self.sx).clamp(0.0, f64::from(self.width)),
            ((f64::from(y1) - self.py) / self.sy).clamp(0.0, f64::from(self.height)),
            ((f64::from(x2) - self.px) / self.sx).clamp(0.0, f64::from(self.width)),
            ((f64::from(y2) - self.py) / self.sy).clamp(0.0, f64::from(self.height)),
        ])
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "ONNX prompt coordinates are FP32 after a bounded finite transform"
    )]
    fn point(&self, [x, y]: [f32; 2]) -> Result<[f32; 2]> {
        ensure!(
            x.is_finite()
                && y.is_finite()
                && x >= 0.0
                && y >= 0.0
                && f64::from(x) <= f64::from(self.width)
                && f64::from(y) <= f64::from(self.height),
            "prompt lies outside the current frame"
        );
        Ok([
            (f64::from(x) * self.sx + self.px) as f32,
            (f64::from(y) * self.sy + self.py) as f32,
        ])
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "tensor sizes are bounded; sample coordinates are clamped to nonnegative image indices before flooring"
    )]
    fn mask(
        &self,
        logits: &[f32],
        mw: usize,
        mh: usize,
        threshold: f32,
        bbox: Option<[f64; 4]>,
    ) -> Result<Vec<u8>> {
        ensure!(
            mw > 0
                && mh > 0
                && mw.checked_mul(mh) == Some(logits.len())
                && logits.iter().all(|v| v.is_finite()),
            "invalid mask tensor"
        );
        let count = usize::try_from(self.width)?
            .checked_mul(usize::try_from(self.height)?)
            .context("mask overflow")?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(count)
            .context("allocating segmentation mask")?;
        pixels.resize(count, 0);
        let cutoff = (f64::from(threshold) / (1.0 - f64::from(threshold))).ln();
        let [left, top, right, bottom] =
            bbox.unwrap_or([0.0, 0.0, f64::from(self.width), f64::from(self.height)]);
        let left = (left - 0.5).ceil().clamp(0.0, f64::from(self.width)) as usize;
        let right = (right - 0.5).ceil().clamp(0.0, f64::from(self.width)) as usize;
        let top = (top - 0.5).ceil().clamp(0.0, f64::from(self.height)) as usize;
        let bottom = (bottom - 0.5).ceil().clamp(0.0, f64::from(self.height)) as usize;
        if left >= right || top >= bottom {
            return Ok(pixels);
        }
        // Horizontal samples are identical for every output row.
        let xs = (left..right)
            .map(|x| {
                let mx = (((x as f64 + 0.5) * self.sx + self.px) * mw as f64
                    / self.model[0] as f64
                    - 0.5)
                    .clamp(0.0, (mw - 1) as f64);
                let x0 = mx.floor() as usize;
                (x0, (x0 + 1).min(mw - 1), mx - x0 as f64)
            })
            .collect::<Vec<_>>();
        for (y, row) in pixels
            .chunks_exact_mut(usize::try_from(self.width)?)
            .enumerate()
            .take(bottom)
            .skip(top)
        {
            let my = (((y as f64 + 0.5) * self.sy + self.py) * mh as f64 / self.model[1] as f64
                - 0.5)
                .clamp(0.0, (mh - 1) as f64);
            let y0 = my.floor() as usize;
            let y1 = (y0 + 1).min(mh - 1);
            let upper = logits
                .get(y0 * mw..(y0 + 1) * mw)
                .context("mask row bounds")?;
            let lower = logits
                .get(y1 * mw..(y1 + 1) * mw)
                .context("mask row bounds")?;
            let dy = my - y0 as f64;
            for (pixel, &(x0, x1, dx)) in row
                .get_mut(left..right)
                .context("mask crop bounds")?
                .iter_mut()
                .zip(&xs)
            {
                let top =
                    f64::from(value(upper, x0)?) * (1.0 - dx) + f64::from(value(upper, x1)?) * dx;
                let bottom =
                    f64::from(value(lower, x0)?) * (1.0 - dx) + f64::from(value(lower, x1)?) * dx;
                *pixel = u8::from(top * (1.0 - dy) + bottom * dy > cutoff);
            }
        }
        Ok(pixels)
    }
}

#[derive(Debug)]
struct Candidate {
    row: usize,
    class: usize,
    score: f32,
    bbox: [f64; 4],
}
fn iou(a: [f64; 4], b: [f64; 4]) -> f64 {
    let [ax, ay, ar, ab] = a;
    let [bx, by, br, bb] = b;
    let overlap = (ar.min(br) - ax.max(bx)).max(0.0) * (ab.min(bb) - ay.max(by)).max(0.0);
    let union = (ar - ax) * (ab - ay) + (br - bx) * (bb - by) - overlap;
    if union > 0.0 { overlap / union } else { 0.0 }
}
fn select(mut candidates: Vec<Candidate>, cap: usize, nms: bool) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut selected: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if candidate.bbox[2] <= candidate.bbox[0] || candidate.bbox[3] <= candidate.bbox[1] {
            continue;
        }
        if nms
            && selected.iter().any(|other| {
                candidate.class == other.class && iou(candidate.bbox, other.bbox) > 0.45
            })
        {
            continue;
        }
        selected.push(candidate);
        if selected.len() == cap {
            break;
        }
    }
    selected
}
fn tensor(outputs: &[Tensor], index: usize) -> Result<&Tensor> {
    outputs.get(index).context("missing segmentation tensor")
}
fn value(values: &[f32], index: usize) -> Result<f32> {
    values.get(index).copied().context("tensor bounds")
}
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "class is validated as a nonnegative integer below the class count"
)]
fn class(value: f32, count: usize) -> Result<usize> {
    ensure!(
        value.is_finite()
            && value >= 0.0
            && value.fract() == 0.0
            && f64::from(value) < f64::from(u32::try_from(count)?),
        "invalid segmentation class id"
    );
    Ok(value as usize)
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the three export layouts and bounded mask generation together"
)]
fn decode_automatic(outputs: &[Tensor], map: &Mapping, options: &Options) -> Result<Vec<Region>> {
    let pixels = usize::try_from(map.width)?
        .checked_mul(usize::try_from(map.height)?)
        .context("mask size overflow")?;
    let cap = options.cap.min(options.maximum / pixels);
    ensure!(cap > 0, "one mask exceeds max-mask-bytes");
    if options.profile.semantic() {
        let masks = tensor(outputs, 0)?;
        let [1, 1, h, w] = masks.shape.as_slice() else {
            anyhow::bail!("BiRefNet requires [1,1,H,W] logits");
        };
        return Ok(vec![Region {
            pixels: map.mask(&masks.data, *w, *h, options.mask_threshold, None)?,
            label: glib::Quark::from_str("foreground"),
            score: -1.0,
            source: None,
            bbox: None,
        }]);
    }
    let classes = options.profile.classes();
    let mut candidates = Vec::new();
    let masks;
    let rows;
    let mut channels = 0;
    let mut columns = 0;
    if options.profile == Profile::EcDetSeg {
        let labels = tensor(outputs, 0)?;
        let boxes = tensor(outputs, 1)?;
        let scores = tensor(outputs, 2)?;
        masks = tensor(outputs, 3)?;
        let [1, q] = labels.shape.as_slice() else {
            anyhow::bail!("ECDetSeg labels must be [1,Q]");
        };
        rows = *q;
        ensure!(
            boxes.shape == [1, rows, 4]
                && scores.shape == [1, rows]
                && masks.shape.len() == 4
                && masks.shape.get(1) == Some(&rows),
            "ECDetSeg query counts differ"
        );
        ensure!(rows <= 100_000, "too many segmentation queries");
        for row in 0..rows {
            let id = class(value(&labels.data, row)?, classes.len())?;
            let score = value(&scores.data, row)?;
            ensure!((0.0..=1.0).contains(&score), "invalid detection confidence");
            let bbox = map.bbox([
                value(&boxes.data, row * 4)?,
                value(&boxes.data, row * 4 + 1)?,
                value(&boxes.data, row * 4 + 2)?,
                value(&boxes.data, row * 4 + 3)?,
            ])?;
            if score >= options.threshold {
                candidates.push(Candidate {
                    row,
                    class: id,
                    score,
                    bbox,
                });
            }
        }
    } else {
        let predictions = tensor(outputs, 0)?;
        masks = tensor(outputs, 1)?;
        let [1, nm, _, _] = masks.shape.as_slice() else {
            anyhow::bail!("YOLO prototypes must be [1,C,H,W]");
        };
        channels = *nm;
        ensure!(
            channels > 0 && channels <= 256,
            "invalid mask coefficient count"
        );
        let [1, a, b] = predictions.shape.as_slice() else {
            anyhow::bail!("YOLO predictions must have rank 3");
        };
        let end_to_end = options.profile == Profile::YoloE26;
        (rows, columns) = if end_to_end { (*a, *b) } else { (*b, *a) };
        ensure!(
            rows <= 100_000 && columns == channels + if end_to_end { 6 } else { 4 + classes.len() },
            "YOLO segmentation class/coefficient layout mismatch"
        );
        for row in 0..rows {
            let get = |column| {
                value(
                    &predictions.data,
                    if end_to_end {
                        row * columns + column
                    } else {
                        column * rows + row
                    },
                )
            };
            let (id, score) = if end_to_end {
                (class(get(5)?, classes.len())?, get(4)?)
            } else {
                let mut best = (0, 0.0f32);
                for id in 0..classes.len() {
                    let score = get(4 + id)?;
                    ensure!((0.0..=1.0).contains(&score), "invalid class confidence");
                    if score > best.1 {
                        best = (id, score);
                    }
                }
                best
            };
            ensure!((0.0..=1.0).contains(&score), "invalid detection confidence");
            let coords = if end_to_end {
                [get(0)?, get(1)?, get(2)?, get(3)?]
            } else {
                let (cx, cy, w, h) = (get(0)?, get(1)?, get(2)?, get(3)?);
                [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0]
            };
            let bbox = map.bbox(coords)?;
            if score >= options.threshold {
                candidates.push(Candidate {
                    row,
                    class: id,
                    score,
                    bbox,
                });
            }
        }
    }
    let [1, _, mh, mw] = masks.shape.as_slice() else {
        anyhow::bail!("mask tensor must have rank 4");
    };
    let plane = mw.checked_mul(*mh).context("mask plane overflow")?;
    ensure!(plane > 0, "empty mask plane");
    let nms = !matches!(options.profile, Profile::EcDetSeg | Profile::YoloE26);
    let candidates = select(candidates, cap, nms);
    let mut regions = Vec::new();
    for candidate in candidates {
        let logits = if options.profile == Profile::EcDetSeg {
            masks
                .data
                .get(candidate.row * plane..(candidate.row + 1) * plane)
                .context("mask row bounds")?
                .to_vec()
        } else {
            let predictions = tensor(outputs, 0)?;
            let end_to_end = options.profile == Profile::YoloE26;
            let start = if end_to_end { 6 } else { 4 + classes.len() };
            let mut logits = vec![0.0f64; plane];
            for c in 0..channels {
                let coefficient = value(
                    &predictions.data,
                    if end_to_end {
                        candidate.row * columns + start + c
                    } else {
                        (start + c) * rows + candidate.row
                    },
                )?;
                let proto = masks
                    .data
                    .get(c * plane..(c + 1) * plane)
                    .context("prototype bounds")?;
                for (logit, value) in logits.iter_mut().zip(proto) {
                    *logit += f64::from(coefficient) * f64::from(*value);
                }
            }
            logits
                .into_iter()
                .map(|v| {
                    ensure!(
                        v.is_finite() && v.abs() <= f64::from(f32::MAX),
                        "mask coefficient accumulation overflow"
                    );
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "checked finite FP64 accumulation converted back to model FP32"
                    )]
                    let value = v as f32;
                    Ok(value)
                })
                .collect::<Result<Vec<_>>>()?
        };
        let pixels = map.mask(
            &logits,
            *mw,
            *mh,
            options.mask_threshold,
            if options.profile == Profile::EcDetSeg {
                None
            } else {
                Some(candidate.bbox)
            },
        )?;
        if pixels.iter().any(|v| *v != 0) {
            regions.push(Region {
                pixels,
                label: glib::Quark::from_str(*classes.get(candidate.class).context("class label")?),
                score: candidate.score,
                source: None,
                bbox: Some(candidate.bbox),
            });
        }
    }
    Ok(regions)
}

#[expect(
    clippy::too_many_lines,
    reason = "explicit encoder/decoder layouts keep model contracts reviewable"
)]
fn run_prompt(
    decoder: &mut Engine,
    profile: Profile,
    features: &[Tensor],
    prompt: &Prompt,
    map: &Mapping,
    options: &Options,
) -> Result<Option<Region>> {
    let mut coords = Vec::new();
    let mut labels = Vec::new();
    if profile != Profile::Sam3
        && let Some([x1, y1, x2, y2]) = prompt.bbox
    {
        coords.extend(map.point([x1, y1])?);
        coords.extend(map.point([x2, y2])?);
        labels.extend([2.0, 3.0]);
    }
    for &(point, positive) in &prompt.points {
        coords.extend(map.point(point)?);
        labels.push(if positive { 1.0 } else { 0.0 });
    }
    let mut args;
    if profile == Profile::Sam3 {
        if labels.is_empty() {
            coords.extend([0.0, 0.0]);
            labels.push(-1.0);
        }
        let points = X::from_shape_vec(&[1, 1, labels.len(), 2], coords)?;
        let labels = X::from_shape_vec(&[1, 1, labels.len()], labels)?;
        let boxes = if let Some([x1, y1, x2, y2]) = prompt.bbox {
            let mut values = map.point([x1, y1])?.to_vec();
            values.extend(map.point([x2, y2])?);
            X::from_shape_vec(&[1, 1, 4], values)?
        } else {
            X::zeros(&[1, 0, 4])
        };
        args = vec![points, labels, boxes];
        for feature in features {
            args.push(feature.x()?);
        }
    } else {
        ensure!(!labels.is_empty(), "empty SAM prompt");
        // SAM's point-only prompt includes the explicit not-a-point sentinel.
        if prompt.bbox.is_none()
            && matches!(profile, Profile::Sam | Profile::MobileSam | Profile::SamHq)
        {
            coords.extend([0.0, 0.0]);
            labels.push(-1.0);
        }
        args = vec![tensor(features, 0)?.x()?];
        if profile == Profile::SamHq {
            args.push(tensor(features, 1)?.x()?.insert_axis(0)?);
        }
        if matches!(profile, Profile::Sam2 | Profile::Sam21) {
            args.push(tensor(features, 1)?.x()?);
            args.push(tensor(features, 2)?.x()?);
        }
        args.push(X::from_shape_vec(&[1, labels.len(), 2], coords)?);
        args.push(X::from_shape_vec(&[1, labels.len()], labels)?);
        if profile != Profile::EdgeSam {
            args.push(X::zeros(&[1, 1, map.model[1] / 4, map.model[0] / 4]));
            args.push(X::zeros(&[1]));
            // Decode in model space, then invert the exact recorded resize/padding.
            args.push(X::from(vec![
                f32::from(u16::try_from(map.model[1])?),
                f32::from(u16::try_from(map.model[0])?),
            ]));
        }
    }
    ensure!(
        args.len() == decoder.inputs.dimss.len(),
        "decoder input count mismatch"
    );
    for (arg, declared) in args.iter().zip(&decoder.inputs.dimss) {
        ensure!(
            arg.shape().len() == declared.len()
                && arg
                    .shape()
                    .iter()
                    .zip(declared)
                    .all(|(a, b)| *b == 0 || a == b),
            "prompt does not match decoder input dimensions"
        );
    }
    let attrs = decoder.outputs.clone();
    let outputs = collect(decoder.run(&args)?, &attrs)?;
    let (masks, scores, object_score) = if profile == Profile::Sam3 {
        let score = value(&tensor(&outputs, 2)?.data, 0)?;
        (
            tensor(&outputs, 1)?,
            tensor(&outputs, 0)?,
            1.0 / (1.0 + (-score).exp()),
        )
    } else if profile == Profile::EdgeSam {
        let mask_index = attrs
            .names
            .iter()
            .position(|name| name == "masks")
            .context("EdgeSAM masks output")?;
        let score_index = attrs
            .names
            .iter()
            .position(|name| name == "scores")
            .context("EdgeSAM scores output")?;
        (
            tensor(&outputs, mask_index)?,
            tensor(&outputs, score_index)?,
            1.0,
        )
    } else {
        (
            tensor(
                &outputs,
                if matches!(profile, Profile::Sam | Profile::MobileSam | Profile::SamHq) {
                    2
                } else {
                    0
                },
            )?,
            tensor(&outputs, 1)?,
            1.0,
        )
    };
    let (count, h, w) = match masks.shape.as_slice() {
        [1, n, h, w] if profile != Profile::Sam3 => (*n, *h, *w),
        [1, 1, n, h, w] if profile == Profile::Sam3 => (*n, *h, *w),
        _ => anyhow::bail!("invalid SAM mask dimensions"),
    };
    ensure!(
        count > 0 && scores.data.len() == count,
        "mask quality count mismatch"
    );
    let (best, quality) = scores
        .data
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .context("mask quality")?;
    let score = quality.clamp(0.0, 1.0) * object_score;
    if score < options.threshold {
        return Ok(None);
    }
    let plane = h.checked_mul(w).context("mask size overflow")?;
    let logits = masks
        .data
        .get(best * plane..(best + 1) * plane)
        .context("SAM mask slice")?;
    let pixels = map.mask(logits, w, h, options.mask_threshold, None)?;
    if pixels.iter().all(|v| *v == 0) {
        return Ok(None);
    }
    Ok(Some(Region {
        pixels,
        label: prompt.label,
        score,
        source: prompt.source,
        bbox: None,
    }))
}

pub(crate) enum Backend {
    Native(Box<Runtime>),
    #[cfg(test)]
    Fixture(fn(&Image, &[Prompt]) -> Result<Vec<Region>>),
}
impl Backend {
    pub fn new(
        model: Option<&Path>,
        encoder: Option<&Path>,
        decoder: Option<&Path>,
        options: Options,
    ) -> Result<Self> {
        Ok(Self::Native(Box::new(Runtime::new(
            model, encoder, decoder, options,
        )?)))
    }
    pub fn run(&mut self, image: &Image, prompts: &[Prompt]) -> Result<Vec<Region>> {
        match self {
            Self::Native(runtime) => runtime.run(image, prompts),
            #[cfg(test)]
            Self::Fixture(infer) => infer(image, prompts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping() -> Mapping {
        Mapping {
            width: 4,
            height: 4,
            model: [4, 4],
            sx: 1.0,
            sy: 1.0,
            px: 0.0,
            py: 0.0,
        }
    }
    fn t(shape: &[usize], data: &[f32]) -> Tensor {
        Tensor {
            shape: shape.to_vec(),
            data: data.to_vec(),
        }
    }
    #[test]
    fn nonsquare_masks_remove_padding_before_threshold() -> Result<()> {
        let map = Mapping {
            height: 2,
            py: 1.0,
            ..mapping()
        };
        let logits = [
            100.0, 100.0, 100.0, 100.0, 4.0, 4.0, -4.0, -4.0, 4.0, 4.0, -4.0, -4.0, 100.0, 100.0,
            100.0, 100.0,
        ];
        ensure!(
            map.mask(&logits, 4, 4, 0.5, None)? == [1, 1, 0, 0, 1, 1, 0, 0],
            "padding leaked into mask"
        );
        ensure!(
            map.mask(&[0.0; 16], 4, 4, 0.5, None)? == [0; 8],
            "zero logits are exactly probability 0.5"
        );
        ensure!(
            map.mask(&[0.0; 16], 4, 4, 0.4, None)? == [1; 8],
            "probability threshold"
        );
        map.mask(&[f32::NAN; 16], 4, 4, 0.5, None)
            .expect_err("nonfinite mask");
        map.mask(&[0.0], 4, 4, 0.5, None)
            .expect_err("invalid dimensions");
        ensure!(
            map.point([2.0, 1.0])?
                .iter()
                .all(|v| (*v - 2.0).abs() < 1e-6),
            "prompt transform"
        );
        map.point([2.0, 3.0]).expect_err("out of frame");
        Ok(())
    }
    #[test]
    fn yolo_nms_keeps_coefficients_attached_to_the_selected_box() -> Result<()> {
        // Two overlapping candidates with opposite coefficients; the higher score wins.
        let outputs = vec![
            t(
                &[1, 6, 2],
                &[2.0, 2.0, 2.0, 2.0, 4.0, 4.0, 4.0, 4.0, 0.6, 0.9, -1.0, 1.0],
            ),
            t(&[1, 1, 2, 2], &[5.0, -5.0, 5.0, -5.0]),
        ];
        let regions = decode_automatic(&outputs, &mapping(), &Options::default())?;
        ensure!(regions.len() == 1, "NMS");
        let region = regions.first().context("region")?;
        ensure!(
            (region.score - 0.9).abs() < 1e-6
                && region.pixels.first() == Some(&1)
                && region.pixels.get(3) == Some(&0),
            "selected coefficient and source alignment"
        );
        let bad = vec![
            t(&[1, 5, 2], &[0.0; 10]),
            outputs.get(1).context("prototype")?.clone(),
        ];
        decode_automatic(&bad, &mapping(), &Options::default()).expect_err("missing coefficients");
        Ok(())
    }
    #[test]
    fn ecdetseg_and_birefnet_outputs_are_bounded_and_validated() -> Result<()> {
        let opts = Options {
            profile: Profile::EcDetSeg,
            ..Options::default()
        };
        let mut outputs = vec![
            t(&[1, 1], &[0.0]),
            t(&[1, 1, 4], &[0.0, 0.0, 4.0, 4.0]),
            t(&[1, 1], &[0.9]),
            t(&[1, 1, 2, 2], &[4.0; 4]),
        ];
        let regions = decode_automatic(&outputs, &mapping(), &opts)?;
        ensure!(
            regions.first().context("region")?.label == glib::Quark::from_str("person"),
            "COCO label"
        );
        outputs.first_mut().context("labels")?.data = vec![80.0];
        decode_automatic(&outputs, &mapping(), &opts).expect_err("invalid class");
        let opts = Options {
            profile: Profile::BiRefDis,
            ..Options::default()
        };
        ensure!(
            decode_automatic(&[t(&[1, 1, 2, 2], &[-4.0; 4])], &mapping(), &opts)?
                .first()
                .context("foreground")?
                .pixels
                == [0; 16],
            "empty foreground"
        );
        decode_automatic(&[t(&[1, 2, 2, 2], &[-4.0; 8])], &mapping(), &opts)
            .expect_err("extra channels");
        decode_automatic(
            &[t(&[1, 1, 2, 2], &[-4.0; 4])],
            &mapping(),
            &Options {
                maximum: 15,
                ..opts
            },
        )
        .expect_err("mask budget");
        Ok(())
    }
    #[test]
    fn prompts_validate_syntax_and_preserve_point_polarity() -> Result<()> {
        let prompt = parse_prompt("box:1,2,3,4;point:2,3;exclude:0,0")?;
        ensure!(
            prompt.bbox == Some([1.0, 2.0, 4.0, 6.0])
                && prompt.points == [([2.0, 3.0], true), ([0.0, 0.0], false)],
            "prompt contents"
        );
        for bad in [
            "",
            "box:1,2,0,4",
            "point:NaN,2",
            "exclude:1,1",
            "box:0,0,1,1;box:1,1,2,2",
            "point:-1,2",
            "text:cat",
        ] {
            parse_prompt(bad).expect_err("invalid prompt");
        }
        parse_prompt(&vec!["point:1,1"; 65].join(";")).expect_err("point bound");
        Ok(())
    }
    #[test]
    fn image_contract_accepts_sizes_and_rejects_dynamic_or_half_inputs() -> Result<()> {
        let mut attrs = OrtTensorAttr {
            names: vec!["images".into()],
            dtypes: vec![TensorElementType::Float32],
            dimss: vec![vec![1, 3, 384, 640]],
            ..OrtTensorAttr::default()
        };
        ensure!(
            image_contract(&attrs, false)? == [640, 384],
            "dimensions from graph"
        );
        attrs.dimss = vec![vec![0, 3, 384, 640]];
        image_contract(&attrs, false).expect_err("dynamic batch");
        attrs.dimss = vec![vec![1, 3, 384, 640]];
        attrs.dtypes = vec![TensorElementType::Float16];
        image_contract(&attrs, false).expect_err("FP16");
        Ok(())
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "tensor sizes are bounded; sample coordinates are clamped to nonnegative image indices before flooring"
    )]
    fn reference_mask(
        map: &Mapping,
        logits: &[f32],
        mw: usize,
        mh: usize,
        threshold: f32,
        bbox: Option<[f64; 4]>,
    ) -> Result<Vec<u8>> {
        ensure!(
            mw > 0
                && mh > 0
                && mw.checked_mul(mh) == Some(logits.len())
                && logits.iter().all(|v| v.is_finite()),
            "invalid mask tensor"
        );
        let count = usize::try_from(map.width)?
            .checked_mul(usize::try_from(map.height)?)
            .context("mask overflow")?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(count)
            .context("allocating segmentation mask")?;
        let cutoff = (f64::from(threshold) / (1.0 - f64::from(threshold))).ln();
        for y in 0..map.height {
            let my = (((f64::from(y) + 0.5) * map.sy + map.py) * mh as f64 / map.model[1] as f64
                - 0.5)
                .clamp(0.0, (mh - 1) as f64);
            let y0 = my.floor() as usize;
            let y1 = (y0 + 1).min(mh - 1);
            for x in 0..map.width {
                if bbox.is_some_and(|[x1, y1, x2, y2]| {
                    f64::from(x) + 0.5 < x1
                        || f64::from(x) + 0.5 >= x2
                        || f64::from(y) + 0.5 < y1
                        || f64::from(y) + 0.5 >= y2
                }) {
                    pixels.push(0);
                    continue;
                }
                let mx = (((f64::from(x) + 0.5) * map.sx + map.px) * mw as f64
                    / map.model[0] as f64
                    - 0.5)
                    .clamp(0.0, (mw - 1) as f64);
                let x0 = mx.floor() as usize;
                let x1 = (x0 + 1).min(mw - 1);
                let at = |xx, yy| -> Result<f64> {
                    Ok(f64::from(
                        *logits.get(yy * mw + xx).context("mask sample bounds")?,
                    ))
                };
                let dx = mx - x0 as f64;
                let dy = my - y0 as f64;
                let top = at(x0, y0)? * (1.0 - dx) + at(x1, y0)? * dx;
                let bottom = at(x0, y1)? * (1.0 - dx) + at(x1, y1)? * dx;
                pixels.push(u8::from(top * (1.0 - dy) + bottom * dy > cutoff));
            }
        }
        Ok(pixels)
    }

    #[test]
    fn masks_match_scalar_reference_at_edges_and_thresholds() -> Result<()> {
        for (width, height) in [(1, 1), (7, 9), (31, 13)] {
            for padded in [false, true] {
                let map = Mapping {
                    width,
                    height,
                    model: [16, 16],
                    sx: 16.0 / f64::from(width),
                    sy: if padded { 8.0 } else { 16.0 } / f64::from(height),
                    px: 0.0,
                    py: if padded { 4.0 } else { 0.0 },
                };
                for (mw, mh) in [(1, 1), (3, 5), (16, 16)] {
                    let logits = (0..mw * mh)
                        .map(|i| f32::from(i16::try_from(i % 17).expect("bounded value")) - 8.0)
                        .collect::<Vec<_>>();
                    for threshold in [0.0, 0.1, 0.5, 0.9, 1.0] {
                        for bbox in [
                            None,
                            Some([0.5, 0.5, 1.5, 1.5]),
                            Some([0.6, 1.2, 5.5, 6.8]),
                            Some([0.0, 0.0, 0.1, 0.1]),
                            Some([0.0, 0.0, f64::from(width), f64::from(height)]),
                        ] {
                            ensure!(
                                map.mask(&logits, mw, mh, threshold, bbox)?
                                    == reference_mask(&map, &logits, mw, mh, threshold, bbox)?,
                                "mask differs: {width}x{height}, {mw}x{mh}, padded={padded}, threshold={threshold}, bbox={bbox:?}"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
