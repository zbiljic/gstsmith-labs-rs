# usls inference plugin

GStreamer inference elements powered by [usls](https://github.com/jamjamjon/usls)
and ONNX Runtime. Both elements attach standard analytics metadata while
preserving the original RGB frame.

| Element | Purpose |
| --- | --- |
| [`uslsobjectdetect`](OBJECT_DETECTION.md) | COCO object detection across supported detector families and sizes |
| [`uslssegment`](SEGMENTATION.md) | Automatic segmentation or masks prompted by same-frame detections and fixed points/boxes |

The element guides cover model profiles, local assets, properties, pipelines
and validation results.

## Build and runtime

GStreamer 1.28 or newer is required. Build and inspect without any model assets:

```sh
cargo build --locked -p gst-plugin-usls
GST_PLUGIN_PATH="$PWD/target/debug" gst-inspect-1.0 uslsobjectdetect
GST_PLUGIN_PATH="$PWD/target/debug" gst-inspect-1.0 uslssegment
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
CoreML. Both elements reject CoreML on those platforms before local model/session
work; this is not a runtime provider fallback. macOS keeps strict distribution
matching and per-session CoreML assignment verification.

## Local tasks

Use `mise tasks --all` to discover workspace and plugin tasks. From the workspace
root or a plugin directory, run `mise run //analytics/usls:TASK`:

| Task           | Purpose                                         |
| -------------- | ----------------------------------------------- |
| `test`         | Ordinary CPU tests; no model or sample required |
| `test:coreml`  | Ordinary tests with optional CoreML support     |
| `build`        | Build the CPU plugin                            |
| `inspect`      | Build and inspect both elements                |
| `smoke:cpu`    | Object detection CPU smoke test                 |
| `smoke:coreml` | Object detection CoreML CPU+GPU smoke test      |
| `smoke:segment` | Explicit segmentation CPU test with local assets |
| `smoke:segment:coreml` | Explicit segmentation CoreML CPU+GPU test |
| `prepare:segment` | Download and prepare FastSAM or MobileSAM smoke assets |
| `fixtures:segment` | Regenerate synthetic segmentation test fixtures |

Smoke tests use explicit local model and sample files and are excluded from the
ordinary test gate. See the [object detection guide](OBJECT_DETECTION.md#validation-with-local-models)
or [segmentation guide](SEGMENTATION.md#validation-and-local-assets) for the
required variables and examples.
