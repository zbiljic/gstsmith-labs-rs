# usls inference plugin

This plugin integrates [usls](https://github.com/jamjamjon/usls)
inference capabilities into GStreamer elements. Its first element,
`uslsobjectdetect`, synchronously attaches standard object detection and related
classification analytics to RGB video. It supports compatible COCO FP32 batch-one
exports across detector sizes through pinned usls and ONNX Runtime 1.28. The
`rfdetr-nano` compatibility profile remains the default. CPU is the portable
default; CoreML CPU+GPU is optional on supported Apple Silicon builds.

GStreamer 1.28 or newer is required. Build and inspect without any model assets:

```sh
cargo build --locked -p gst-plugin-usls
GST_PLUGIN_PATH="$PWD/target/debug" gst-inspect-1.0 uslsobjectdetect
cargo build --locked -p gst-plugin-usls --features coreml
```

The build deliberately downloads the matching ORT binary. Plugin registration,
state changes and streaming never download weights, labels or fonts. An existing
compatible ORT environment can be shared for CPU. CoreML requires this plugin's
process-wide logger to observe each session's final ORT provider assignment; if
another component initialized ORT first without it, CoreML fails explicitly.
Multiple usls elements and restarts reuse the logger with separate initialization
capture. All-CPU ORT fallback is rejected. CoreML partitions may retain ORT CPU
nodes; CoreML's internal CPU/GPU/ANE placement is not exposed by pinned usls.

Static cargo-c consumers on macOS need Clang's runtime directory in their link
search path (`-L"$(cc --print-runtime-dir)"`), because the downloaded ORT archive
requires `clang_rt.osx`. The package checker verifies that runtime exists and
uses this SDK path. This was tested on macOS 26.5.1; linker warnings about ORT
objects targeting 26.5 versus the consumer's 26.0 target mean this run does not
qualify deployment on older macOS versions.

On non-macOS targets, a target-scoped ORT `lax-feature-matching` setting lets
`--all-features` select the available CPU distribution despite Cargo forwarding
CoreML. The element rejects CoreML on those platforms before local model/session
work; this is not a runtime provider fallback. macOS keeps strict distribution
matching and per-session CoreML assignment verification.

## Local models

Select a detector family and supply a local ONNX file. The static image size
comes from ONNX input declarations; output dimensions are checked against the
declarations and actual inference tensors. Changing from a small model to a
larger compatible export does not require a new profile. For example, RF-DETR
Nano uses 384×384 while Large 2026 uses 704×704; both use
`model-profile=rfdetr-coco` with their corresponding local `model-file`.

| `model-profile`       | Models with this preprocessing and output contract | Compatibility alias |
| --------------------- | -------------------------------------------------- | ------------------- |
| `rfdetr-coco`         | RF-DETR COCO detection                             | `rfdetr-nano`       |
| `rtdetr-v1-coco`      | RT-DETR v1 COCO                                    | `rtdetr-v1-r18`     |
| `rtdetr-v2-coco`      | RT-DETR v2 COCO                                    | `rtdetr-v2-s`       |
| `rtdetr-v4-coco`      | RT-DETR v4 COCO                                    | `rtdetr-v4-s`       |
| `picodet-coco`        | PP-PicoDet COCO with embedded NMS                  | `picodet-l-coco`    |
| `dfine-coco`          | D-FINE COCO                                        | `dfine-n-coco`      |
| `deim-dfine-coco`     | DEIM D-FINE COCO                                   | `deim-dfine-s-coco` |
| `deimv2-hgnetv2-coco` | DEIMv2 HGNetv2: Atto, Femto, Pico, N               | `deimv2-atto-coco`  |
| `deimv2-dinov3-coco`  | DEIMv2 DINOv3: S, M, L, X                          | —                   |

Pipeline smoke tests passed on CPU and CoreML CPU+GPU for RF-DETR Large 2026,
RT-DETR v1 R50, v2 L, v4 L, D-FINE L, DEIM D-FINE L, and DEIMv2 HGNetv2 N and
DINOv3 S. Other sizes are accepted when their contracts match, but have not each
been separately qualified.

The aliases preserve their enum values and preprocessing, but no longer restrict
files to one exact artifact. All profiles permit CPU and optional CoreML
CPU+GPU; a CoreML session must actually assign work to CoreML. Only the legacy
`rfdetr-nano` profile permits the previously qualified `all` and
`cpu-and-neural-engine` settings. Other profiles reject those settings.

ONNX provides names, dtypes and shapes, but these exports do not consistently
include class names or preprocessing metadata. The explicit profile selects
normalization, resize behavior, decoding and COCO label semantics. In particular,
DEIMv2 DINOv3 needs [ImageNet mean/std normalization][deimv2-dino], while HGNetv2
only scales pixels to 0..1. Their identical output layouts cannot identify that
difference. Custom classes, Objects365 and segmentation exports are unsupported.

Startup requires a readable local regular file with a static square FP32 NCHW
image input `[1,3,H,H]`, with `H` in 1..65535. The auxiliary input, where present,
must be `[1,2]`.
Dynamic input dimensions and FP16 inputs are rejected. Supported output contracts
are:

- RF-DETR: `pred_boxes` / `pred_logits`, or `boxes` / `logits`, with shapes
  `[1,Q,4]` / `[1,Q,91]` and FP32 values.
- RT-DETR, D-FINE and DEIM families: `labels` / `boxes` / `scores`, with shapes
  `[1,Q]` / `[1,Q,4]` / `[1,Q]`, respectively INT64 / FP32 / FP32.
- PicoDet: `multiclass_nms3_0.tmp_0` / `multiclass_nms3_0.tmp_2`, with shapes
  `[Q,6]` / `[1]` and FP32 / INT32 values.

Symbolic output dimensions are allowed and checked against actual tensors on
every inference. Extra outputs, inconsistent row counts, invalid class IDs and
nonfinite results fail explicitly. Tensor compatibility does not establish a
model's accuracy or preprocessing semantics; choose the profile that matches
its export. No model hash allowlist is enforced.

### Prepare a larger model

Download and specialization are explicit steps outside plugin operation. This
example uses RF-DETR Large 2026 (704×704). With ONNX Runtime tools 1.28.0 and ONNX
1.23.2 installed in an opt-in Python environment, fix its dynamic batch to one:

```sh
sh <<'SH'
set -eu
MODEL_DIR=target/models
mkdir -p "$MODEL_DIR"
curl --fail --location --retry 3 \
  https://github.com/jamjamjon/assets/releases/download/rfdetr/large-2026.onnx \
  --output "$MODEL_DIR/rfdetr-large-2026.onnx"
printf '%s  %s\n' \
  3771258fe91c75b10dbbdc0359b129320d556740d7b7b08c40a7fc37b1610ae9 \
  "$MODEL_DIR/rfdetr-large-2026.onnx" | shasum -a 256 --check
python3 -m onnxruntime.tools.make_dynamic_shape_fixed \
  --dim_param batch --dim_value 1 \
  "$MODEL_DIR/rfdetr-large-2026.onnx" "$MODEL_DIR/rfdetr-large-2026-batch1.onnx"
printf '%s  %s\n' \
  b891b1ba7acab5de932e192a8565baf030f534fd6739eafcd5e90ebe97d7c3f9 \
  "$MODEL_DIR/rfdetr-large-2026-batch1.onnx" | shasum -a 256 --check
SH
```

The hashes identify this recipe's input and output for reproducibility. For other
exports, inspect the ONNX inputs and fix each symbolic batch dimension to one;
the names can differ between families and between the image and auxiliary
inputs. Preserve the exported spatial dimensions, operators and weights. A graph
that already has static batch-one inputs needs no specialization.

The publisher does not record exact export-source revisions or a separate
weight-license grant for these release assets. The plugin's Apache-2.0 license
does not establish a license for the weights.

RF-DETR uses RGB bilinear letterbox with padding 114, normalization /255,
mean `[0.485,0.456,0.406]`, std `[0.229,0.224,0.225]`. Image processing follows
the pinned usls presets, with the DINOv3 normalization described above. The
plugin validates and decodes output tensors locally, preserving each family's
coordinate conventions. RT-DETR and RF-DETR reverse the recorded letterbox
transform; PicoDet outputs already use source-image coordinates.

The plugin directly reuses usls's `NAMES_COCO_80` labels. Dense class IDs 0..79
index them directly; RF-DETR's sparse category IDs 1..90 use an explicit map,
rejecting background and unused IDs. The [official RF-DETR 1.2.1 dictionary][rf-coco]
assigns `book` to category ID 84 and leaves 83 unused. The [pinned usls COCO91
table][usls-coco91] puts `book` at 83 instead, so that table cannot replace the
sparse map. No upstream constants are changed and no runtime label file is needed.
The publisher's exact model-export revision is unknown.

The element validates finite detections, clamps both endpoints independently
and rounds analytics boxes outward within the source frame. RF-DETR computes
both endpoints before clipping, preserving geometry for boxes crossing any edge.

[deimv2-dino]: https://github.com/Intellindust-AI-Lab/DEIMv2/blob/1d2ca42171570c713e78fc6a766ec5104b7f4724/configs/deimv2/deimv2_dinov3_s_coco.yml
[rf-coco]: https://github.com/roboflow/rf-detr/blob/fb4ceea0d5750890e43964f5fc558c4caf9b6f38/rfdetr/util/coco_classes.py
[usls-coco91]: https://github.com/jamjamjon/usls/blob/c15ad77c1879dbf1117376d34d3022a325cd9cdc/src/models/names/coco_91.rs

## Properties and streaming

All settings are mutable through READY. Sessions initialize once per run before
frames and are discarded at stop. Settings cannot change during streaming.

| Property               | Default       | Contract                                                            |
| ---------------------- | ------------- | ------------------------------------------------------------------- |
| `model-profile`        | `rfdetr-nano` | Detector family and preprocessing from the table above              |
| `model-file`           | unset         | Required local compatible graph matching `model-profile`            |
| `score-threshold`      | `0.5`         | Finite 0..1                                                         |
| `max-detections`       | `100`         | 1..1000; stable descending-score cap                                |
| `max-frame-bytes`      | `33554432`    | 1..134217728; bounds both carrier mapping and packed RGB allocation |
| `execution-provider`   | `cpu`         | `cpu` or `coreml`; unavailable provider is an error                 |
| `coreml-compute-units` | `cpu-and-gpu` | `all`, `cpu-and-gpu`, `cpu-and-neural-engine`                       |

The `model-file` must match the selected profile's preprocessing and COCO classes.
The model size is read from the file at startup.
ORT threading is fixed at 10 intra / 1 inter threads. CoreML uses MLProgram, static
inputs, subgraphs enabled and FastPrediction specialization. Compute-unit names
request allowed hardware; they do not prove Neural Engine execution.

Always-present pads accept system-memory `video/x-raw,format=RGB` with square
pixels and identical input/output dimensions. VideoFrame/VideoMeta stride and
offset are honored, including checked negative-stride rows. The carrier and row
extents are checked before mapping/allocation. RGB rows are packed, then usls
owns another image copy; this is not zero-copy. Original pixel bytes, timestamps,
duration, offsets, flags and prior metadata remain on the original carrier.
Existing analytics relation metadata is reused. Each surviving box has one
object detection and one matching classification linked by `RELATE_TO`.

One frame completes inference and metadata before downstream release. Both
providers apply normal backpressure; there is no sampling, internal queue or
async mode. Empty results succeed. Nonfinite scores/coordinates, invalid classes
and upstream inference errors fail the flow. Startup errors fail state changes.
Upstream Rust unwinds are caught and the affected runtime is discarded; native
abort, cancellation and a stuck native inference cannot be recovered by this
boundary. Shutdown waits for synchronous native work.

A finite JPEG pipeline using the default threshold:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" \
gst-launch-1.0 \
  filesrc location=sample.jpg \
  ! jpegdec \
  ! videoconvert \
  ! 'video/x-raw,format=RGB,pixel-aspect-ratio=1/1' \
  ! uslsobjectdetect model-profile=rfdetr-coco \
      model-file=target/models/rfdetr-large-2026-batch1.onnx \
  ! objectdetectionoverlay \
  ! fakesink sync=false
```

For the CoreML build, add `execution-provider=coreml` and
`coreml-compute-units=cpu-and-gpu` to the element.

## Local tasks

Use `mise tasks --all` to discover workspace and plugin tasks. From the workspace
root or a plugin directory, run `mise run //analytics/usls:TASK`:

| Task           | Purpose                                         |
| -------------- | ----------------------------------------------- |
| `test`         | Ordinary CPU tests; no model or sample required |
| `test:coreml`  | Ordinary tests with optional CoreML support     |
| `build`        | Build the CPU plugin                            |
| `inspect`      | Build and inspect `uslsobjectdetect`            |
| `smoke:cpu`    | Explicit real-model CPU test                    |
| `smoke:coreml` | Explicit real-model CoreML CPU+GPU test         |

The smoke tasks require `GSTSMITH_USLS_MODEL` and `GSTSMITH_USLS_SAMPLE` to name
existing local files. `GSTSMITH_USLS_PROFILE` selects a profile and defaults to
`rfdetr-nano`; select a family profile for a larger compatible model.
They never download assets and are excluded from ordinary tests and the
workspace pre-commit gate. They check person detections, source
coordinates, classification relations, original pixels/timing, two elements each
restarted, standard overlay compatibility and finite EOS at threshold `0.55`.

The retained person sample is COCO val2017 image 139:
[000000000139.jpg](https://s3.amazonaws.com/images.cocodataset.org/val2017/000000000139.jpg),
SHA256 `ffe0f0cec3b2e27aab1967229cdf0a0d7751dcdd5800322f0b8ac0dffb3b8a8d`.
COCO identifies its image license as license 2,
[Attribution-NonCommercial 2.0](http://creativecommons.org/licenses/by-nc/2.0/).
No dataset is needed for this smoke test. Download the single image explicitly
and verify its SHA256 before use.

Run this example from the workspace root, or provide absolute local asset paths.

```sh
GSTSMITH_USLS_PROFILE=rfdetr-coco \
GSTSMITH_USLS_MODEL="$PWD/target/models/rfdetr-large-2026-batch1.onnx" \
GSTSMITH_USLS_SAMPLE="$PWD/target/qualification/images/000000000139.jpg" \
mise run //analytics/usls:smoke:cpu
# Select a family and its local model file to smoke-test another size.
# With the same local asset variables, use smoke:coreml for CoreML CPU+GPU.
```
