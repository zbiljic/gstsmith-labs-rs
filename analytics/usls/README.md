# usls inference plugin

This plugin integrates [usls](https://github.com/jamjamjon/usls)
inference capabilities into GStreamer elements. Its first element,
`uslsobjectdetect`, synchronously attaches standard object detection and related
classification analytics to RGB video. It supports eight qualified FP32 batch-one
detector profiles through pinned usls and ONNX Runtime 1.28. RF-DETR Nano remains
the default. CPU is the portable default; CoreML CPU+GPU is optional on supported
Apple Silicon builds.

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

All profiles below support CPU and CoreML CPU+GPU on supported Apple Silicon
builds. Only RF-DETR Nano also permits the previously qualified CoreML `all` and
`cpu-and-neural-engine` settings; other profiles reject those settings as
unqualified. A profile must be paired with its exact accepted graph.

Download and specialization are explicit preparation steps outside plugin
operation. Release suffixes below follow
`https://github.com/jamjamjon/assets/releases/download/`. Dimensions shown are
FP32 NCHW image inputs at batch one; only the named batch dimensions are fixed.

| `model-profile`     | Release suffix                  | Image size | Batch dimension(s)   |
| ------------------- | ------------------------------- | ---------- | -------------------- |
| `rfdetr-nano`       | `rfdetr/nano.onnx`              | 384×384    | `batch`              |
| `rtdetr-v1-r18`     | `rtdetr/v1-r18.onnx`            | 640×640    | `Batch`              |
| `rtdetr-v2-s`       | `rtdetr/v2-s.onnx`              | 640×640    | `Batch`              |
| `rtdetr-v4-s`       | `rtdetr/v4-s.onnx`              | 640×640    | `Batch`              |
| `picodet-l-coco`    | `picodet/l-coco.onnx`           | 640×640    | `DynamicDimension.0` |
| `dfine-n-coco`      | `d-fine/n-coco.onnx`            | 640×640    | `N unk__409`         |
| `deim-dfine-s-coco` | `deim/dfine-s-coco.onnx`        | 640×640    | `N unk__479`         |
| `deimv2-atto-coco`  | `deimv2/hgnetv2-atto-coco.onnx` | 320×320    | `N`                  |

The original and accepted SHA256 values pin each recipe's input and output.
Startup checks local regular-file status, byte length, streaming SHA256 and the
actual model-specific tensor names, dtypes and dimensions. Dynamic originals,
other exports and FP16 variants are rejected.

| Profile             | Original SHA256                                                    | Accepted batch-one SHA256                                          | Accepted bytes |
| ------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ | -------------- |
| `rfdetr-nano`       | `ba3439c50ead0721a4b0ea8086c9ed78dfeb7426574f359900c1fcd3e7a1e291` | `9bbecf62d2ac0718cdd9d36cc30fd013d6efdd00e887cd66765ac077bc7177b1` | 108,050,760    |
| `rtdetr-v1-r18`     | `39693dae49de06af52a476473c8a7423829b0697f58cc382ef085fd90dd52a98` | `79875f7b608d06e065f50ea473a5887537b93f8abd60c1c9060fd37c8f23a8e4` | 80,464,702     |
| `rtdetr-v2-s`       | `3370e6c17b8f83f62eee3a0216ec6e417d79b66108ccdf6704de552e625a1e0b` | `c75afbcebb58e1b8016ee6c1a09f879995e4ed273707572e2c5ee9528a26948a` | 80,447,976     |
| `rtdetr-v4-s`       | `e86d6fe7aa32b291418aa4aa1ae74380f0bf4be5154f62a57fafdfe2eeb9635e` | `8b8931caa850d2c9712ca90851bb856a43a3a1a0faf5862567ace7c0b725ddd2` | 41,770,934     |
| `picodet-l-coco`    | `3893e05556969bafdaac80c20648bd54a5feee279ed070892863c2600635b313` | `5255b1a46031f717c7cc1e9ca2c1ac74117f6206e30eba27d4f9aa23ac20ecac` | 23,602,032     |
| `dfine-n-coco`      | `bfb92613b75e92a952f249535847e743f81fd279ffc3e7bffc8d662d21e59b1a` | `537c995799e8391215de8e7d2e5d3469787ea4ae7e53068751fabd6e2c0db9e3` | 15,501,246     |
| `deim-dfine-s-coco` | `2b71a81cf05be32bc8f3bd50a4346cba55b2b7b68e1a49ccc06e4fa552d394f7` | `78345b185cc28238b35cf2ec5721e86f2678948aed5ef7b572c6e59fe7b73efd` | 41,761,973     |
| `deimv2-atto-coco`  | `f7b70694ad8786d545bd6e496b74261f5da8a1207ce83dcf63a44c0de7a0ebf5` | `5776c9b6fe8baa74b686d800265e44cb084d1fea4712c6ae43cf8d16ab857b5a` | 2,192,333      |

Use ONNX Runtime tools 1.28.0 and ONNX 1.23.2 in an opt-in Python environment.
This shared recipe defaults to RF-DETR; select another row's profile, release,
batch dimensions and hashes to prepare it. It preserves operators, weights and
spatial dimensions. D-FINE and DEIM require both listed batch symbols to be fixed.

```sh
sh <<'SH'
set -eu
PROFILE=rfdetr-nano
RELEASE=rfdetr/nano.onnx
BATCH_DIMS='batch'
ORIGINAL_SHA=ba3439c50ead0721a4b0ea8086c9ed78dfeb7426574f359900c1fcd3e7a1e291
ACCEPTED_SHA=9bbecf62d2ac0718cdd9d36cc30fd013d6efdd00e887cd66765ac077bc7177b1
MODEL_DIR=target/qualification/models
mkdir -p "$MODEL_DIR"
curl --fail --location --retry 3 \
  "https://github.com/jamjamjon/assets/releases/download/$RELEASE" \
  --output "$MODEL_DIR/$PROFILE.onnx"
printf '%s  %s\n' "$ORIGINAL_SHA" "$MODEL_DIR/$PROFILE.onnx" | shasum -a 256 --check
MODEL_STAGE="$MODEL_DIR/$PROFILE.onnx"
for BATCH_DIM in $BATCH_DIMS; do
  MODEL_NEXT="$MODEL_DIR/$PROFILE-batch1-$BATCH_DIM.onnx"
  python3 -m onnxruntime.tools.make_dynamic_shape_fixed \
    --dim_param "$BATCH_DIM" --dim_value 1 "$MODEL_STAGE" "$MODEL_NEXT"
  MODEL_STAGE="$MODEL_NEXT"
done
mv "$MODEL_STAGE" "$MODEL_DIR/$PROFILE-batch1.onnx"
printf '%s  %s\n' "$ACCEPTED_SHA" "$MODEL_DIR/$PROFILE-batch1.onnx" | shasum -a 256 --check
SH
```

The publisher does not record exact export-source revisions or a separate
weight-license grant for these release assets. The plugin's Apache-2.0 license
does not establish a license for the weights.

RF-DETR uses RGB bilinear letterbox with padding 114, normalization /255,
mean `[0.485,0.456,0.406]`, std `[0.229,0.224,0.225]`. Its FP32 outputs are
`pred_boxes [1,queries,4]` and `pred_logits [1,queries,91]` (observed 300 queries).
RT-DETR and its D-FINE/DEIM aliases return INT64 labels, FP32 boxes and scores;
PicoDet returns FP32 detection rows and INT32 counts. Preprocessing uses each
pinned usls preset. RT-DETR and PicoDet retain upstream decoding; RF-DETR decodes
raw boxes locally using the recorded letterbox geometry.

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

[rf-coco]: https://github.com/roboflow/rf-detr/blob/fb4ceea0d5750890e43964f5fc558c4caf9b6f38/rfdetr/util/coco_classes.py
[usls-coco91]: https://github.com/jamjamjon/usls/blob/c15ad77c1879dbf1117376d34d3022a325cd9cdc/src/models/names/coco_91.rs

## Properties and streaming

All settings are mutable through READY. Sessions initialize once per run before
frames and are discarded at stop. Settings cannot change during streaming.

| Property               | Default       | Contract                                                            |
| ---------------------- | ------------- | ------------------------------------------------------------------- |
| `model-profile`        | `rfdetr-nano` | Qualified detector profile from the table above                     |
| `model-file`           | unset         | Required local exact graph matching `model-profile`                 |
| `score-threshold`      | `0.5`         | Finite 0..1                                                         |
| `max-detections`       | `100`         | 1..1000; stable descending-score cap                                |
| `max-frame-bytes`      | `33554432`    | 1..134217728; bounds both carrier mapping and packed RGB allocation |
| `execution-provider`   | `cpu`         | `cpu` or `coreml`; unavailable provider is an error                 |
| `coreml-compute-units` | `cpu-and-gpu` | `all`, `cpu-and-gpu`, `cpu-and-neural-engine`                       |

Changing `model-profile` requires a matching `model-file` before startup.
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
  ! uslsobjectdetect model-file=target/qualification/models/rfdetr-nano-batch1.onnx \
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
`rfdetr-nano`; the same retained person sample works for all eight profiles.
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
GSTSMITH_USLS_PROFILE=rfdetr-nano \
GSTSMITH_USLS_MODEL="$PWD/target/qualification/models/rfdetr-nano-batch1.onnx" \
GSTSMITH_USLS_SAMPLE="$PWD/target/qualification/images/000000000139.jpg" \
mise run //analytics/usls:smoke:cpu
# Set GSTSMITH_USLS_PROFILE and GSTSMITH_USLS_MODEL for another table row.
# With the same local asset variables, use smoke:coreml for CoreML CPU+GPU.
```
