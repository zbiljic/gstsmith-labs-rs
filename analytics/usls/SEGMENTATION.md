# uslssegment

`uslssegment` synchronously attaches GStreamer 1.28 segmentation metadata to
RGB frames. Automatic models segment each frame; prompted models take boxes
from detection metadata on that same buffer, or a fixed box/point prompt.
Original pixels, timestamps and existing metadata pass through unchanged.

## Models

Select the export family explicitly. Input size comes from the local ONNX graph,
so compatible model sizes share a profile. These profiles cover the **Image
Segmentation** table in [usls](https://github.com/jamjamjon/usls).

| Model family | `model-profile` | Input assets / prompts |
| --- | --- | --- |
| SAM | `sam` | Encoder + decoder; box/points |
| SAM-HQ | `sam-hq` | Encoder + decoder; box/points |
| MobileSAM | `mobile-sam` | Encoder + decoder; box/points |
| EdgeSAM | `edge-sam` | Encoder + decoder; box/points |
| SAM2 2.0 / 2.1 | `sam2` / `sam2.1` | Encoder + decoder; box/points |
| SAM3-Tracker | `sam3-tracker` | Vision encoder + prompt/mask decoder; box/points |
| YOLOE v8 / 11 Prompt-Free | `yoloe-v8-pf` / `yoloe-11-pf` | Single graph; automatic, 4585 classes |
| YOLOE26 Prompt-Free | `yoloe-26-pf` | Single one-to-one graph; automatic, 4585 classes |
| FastSAM | `fastsam` (default) | Single graph; automatic, class `object` |
| EdgeCrafter ECDetSeg | `ecdetseg-coco` | Single graph; automatic, COCO 80 classes |
| BiRefNet COD | `birefnet-cod` | Single graph; foreground/background |
| BiRefNet DIS | `birefnet-dis` | Single graph; foreground/background |
| BiRefNet HRSOD | `birefnet-hrsod` | Single graph; foreground/background |
| BiRefNet Massive | `birefnet-massive` | Single graph; foreground/background |

Compatible exports require static FP32 NCHW image input `[1,3,H,W]`. Dynamic
image dimensions/batch must be specialized outside the plugin. Decoder point
counts and output dimensions may be symbolic and are checked on each call.
Encoder/decoder pairs must match. FP16 image inputs, custom classes, text
prompts, RF-DETR segmentation and the separate matting/background-removal
families are outside this element's profiles.

Preprocessing follows the usls family configuration. FastSAM and YOLOE
use prototype masks plus coefficients; v8/11 apply class-aware NMS at IoU 0.45,
while YOLOE26 uses its one-to-one predictions. ECDetSeg consumes its labels,
boxes, scores and mask logits. BiRefNet produces one semantic foreground mask.
SAM variants select the highest predicted-IoU mask for each prompted object.
Mask logits are bilinearly mapped through the recorded resize/padding to the
source frame before thresholding. No label/font/model downloads occur at runtime.

## Prompts in a pipeline

The default prompt source is `AnalyticsODMtd` on the current RGB buffer. Each
eligible detection supplies one box and retains its class label. The image
encoder runs once per frame; the mask decoder runs once per selected object.
No detections means no new masks. Every frame starts with its own prompts,
including the `sam3-tracker` profile: there is no persistent video tracking.

With local compatible detector and MobileSAM graphs:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  filesrc location=sample.jpg ! jpegdec ! videoconvert \
  ! 'video/x-raw,format=RGB,pixel-aspect-ratio=1/1' \
  ! uslsobjectdetect model-profile=rfdetr-coco model-file=detector.onnx \
  ! uslssegment model-profile=mobile-sam \
      encoder-file=mobile-encoder-batch1.onnx decoder-file=mobile-decoder.onnx \
  ! segmentationoverlay ! fakesink sync=false
```

A fixed prompt overrides detection metadata and describes one object in source
pixel coordinates. Example (requires a frame at least 300×320):

```sh
# Replace the uslssegment portion of the pipeline with:
uslssegment model-profile=mobile-sam \
  encoder-file=mobile-encoder-batch1.onnx decoder-file=mobile-decoder.onnx \
  prompt='box:100,80,200,240;point:160,160;exclude:110,90'
```

`box:x,y,width,height`, `point:x,y` and `exclude:x,y` may be combined, separated
by semicolons. One box and up to 64 points are permitted; a box or positive point
is required. Coordinates must be finite, nonnegative and within the current
frame. Fixed prompts are mutable through READY, so changing one requires a state
transition. Detection prompts naturally change on every buffer.

An automatic model needs only one graph:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  filesrc location=sample.jpg ! jpegdec ! videoconvert \
  ! 'video/x-raw,format=RGB,pixel-aspect-ratio=1/1' \
  ! uslssegment model-profile=fastsam model-file=FastSAM-s-batch1.onnx \
  ! segmentationoverlay ! fakesink sync=false
```

## Properties and limits

All properties are mutable through READY only.

| Property | Default | Meaning |
| --- | --- | --- |
| `model-profile` | `fastsam` | Family from the table above |
| `model-file` | unset | Required local graph for automatic profiles |
| `encoder-file`, `decoder-file` | unset | Required local graph pair for prompted profiles |
| `prompt` | unset | Fixed prompt; otherwise use current-buffer detections |
| `score-threshold` | `0.5` | Detection confidence and predicted mask quality cutoff, 0..1 |
| `mask-threshold` | `0.5` | Foreground probability cutoff, 0..1 |
| `max-regions` | `16` | Highest-confidence detections/prompts to process, 1..1000 |
| `max-mask-bytes` | `67108864` | Total source-sized mask storage per frame; maximum 512 MiB |
| `max-frame-bytes` | `33554432` | Bounds carrier mapping and packed RGB copy; maximum 128 MiB |
| `execution-provider` | `cpu` | CPU, or optional CoreML for automatic profiles |
| `coreml-compute-units` | `cpu-and-gpu` | Only supported segmentation CoreML setting |

Mask storage further caps the number of processed objects. A single mask that
exceeds the budget fails the flow. A GRAY8 mask costs `width × height` bytes;
16 masks at 1920×1080 cost about 32 MiB, in addition to the original frame,
image copies and inference tensors. Model input allocation is capped at 128 MiB,
and copied output tensors at 256 MiB per session invocation. These bounds do
not limit ORT's internal allocations or the model's working memory. Use trusted
local graphs. Frame and model dimensions must each be at most 65535.

Pads accept system-memory RGB with square pixels and identical input/output
caps. VideoMeta stride/offset, including negative strides, are honored. This
is synchronous inference with normal backpressure and no internal frame queue;
multiple prompted objects increase per-frame latency. Errors in startup fail the
state change; malformed/nonfinite results fail the flow. Rust panics discard the
runtime; native aborts or stuck native calls cannot be recovered here.

CPU supports every profile. FastSAM-S also passed the CoreML CPU+GPU smoke test
on Apple Silicon with the optional `coreml` build and verified ORT assignment.
Other automatic models can use CoreML when ORT assigns work to it; CPU-only
fallback is rejected. Prompted profiles currently require CPU.

## Metadata

Each selected object gets a separate full-frame binary GRAY8
`GstAnalyticsSegmentationMtd`, preserving overlapping objects. Region 0 is
background and region 1 foreground. A related classification has matching
`background` and object label entries. Instance models link an object detection
to its mask with `RELATE_TO`; prompted masks link to the original detection ID.
Fixed prompts have no source detection. BiRefNet produces semantic metadata
with class `foreground` and unknown confidence (-1).

Consumers use standard analytics metadata, including `segmentationoverlay`.
The small [segmentation-meta](../../utils/segmentation-meta/) crate supplies the
safe binding missing from gstreamer-analytics 0.25.4. Its isolated FFI boundary
owns mask buffers correctly across carrier copies; plugin crates retain their
`unsafe_code = forbid` policy.

## Validation and local assets

Ordinary tests run without external assets:

```sh
cargo test --locked -p gst-plugin-usls -p segmentation-meta --all-targets
mise run //analytics/usls:inspect
```

Synthetic ONNX fixtures exercise every profile's actual ORT sessions, mask
layout and prompt relations, alongside geometry, thresholds, limits, lifecycle,
carrier preservation, overlapping masks and standard overlay tests. The small
synthetic graphs are checked in so ordinary Rust tests need no Python or model
downloads. They establish tensor compatibility, not pretrained accuracy. See
[fixture maintenance](tests/fixtures/segment/README.md) for regeneration.

Real-model CPU smoke tests passed for FastSAM-S (640×640) and MobileSAM
(1024×1024), with two elements each restarted, source-sized nonempty masks,
original pixels/timing, same-buffer prompt relations, overlay and EOS. Other
families/sizes have contract tests but have not each been qualified with weights.
FastSAM-S passed the same checks on CoreML CPU+GPU, with the entire ORT graph
assigned to CoreML. This verifies provider execution, not CoreML's internal
CPU/GPU placement or a performance target.
The opt-in smoke test expects a sample that produces a nonempty mask:

```sh
GSTSMITH_SEGMENT_PROFILE=fastsam \
GSTSMITH_SEGMENT_MODEL="$PWD/target/models/FastSAM-s-batch1.onnx" \
GSTSMITH_SEGMENT_SAMPLE="$PWD/sample.jpg" \
mise run //analytics/usls:smoke:segment
```

Use `smoke:segment:coreml` with the same FastSAM asset variables to run the
CoreML check; the task enables the `coreml` feature.

For prompted models set `GSTSMITH_SEGMENT_ENCODER` and
`GSTSMITH_SEGMENT_DECODER` instead of `GSTSMITH_SEGMENT_MODEL`. The test supplies
a whole-frame detection box. `GSTSMITH_SEGMENT_PROMPT` can override it.

To download, verify and prepare the tested assets, run either task from the
workspace root (requires `uv` and `curl`):

```sh
mise run //analytics/usls:prepare:segment fastsam
mise run //analytics/usls:prepare:segment mobile-sam
```

Files are written to `target/models/`; use `--output-dir PATH` to change it.
The task fixes image inputs to batch one at the tested sizes and preserves the
MobileSAM decoder's dynamic prompt dimensions. Existing files are verified and
reused. Asset URLs and input/output checksums live in the
[preparation script](scripts/prepare_segment.py). Optional tooling dependencies
are managed by the [mise tasks](mise.toml).
