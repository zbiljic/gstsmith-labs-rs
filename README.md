# gstsmith-labs-rs

Repository containing experimental
[GStreamer](https://gstreamer.freedesktop.org/) plugins written in Rust.
Plugins remain here while their behavior, portability, and operational limits
are being established before promotion to
[`gstsmith-rs`](https://github.com/zbiljic/gstsmith-rs).

## Plugins

- [`generic`](generic/)

  - [`jsontap`](generic/jsontap/): Passthrough inspection tap.
    - `jsontap`: Record buffer timestamps, flags, sizes, and metadata, caps
      changes, and EOS as JSON lines while preserving downstream data.

  - [`ptssampler`](generic/ptssampler/): Running-time period sampling.
    - `ptssampler`: Pass the first buffer of every period and drop the rest,
      preserving buffers and caps.

- [`analytics`](analytics/)

  - [`cactus`](analytics/cactus/): Elements running models on the
    [Cactus](https://cactuscompute.com) engine.
    - `cactusmirage`: Describe frames with the Mirage vision model: objects
      with outlines, recognized text, relations, captions and form answers
      as analytics metadata, plus the engine's full JSON.
    - `cactuswhistle`: Transcribe speech with the Whistle model into timed
      text cues, in rolling windows or with the engine's streaming
      transcriber.

  - [`nalsense`](analytics/nalsense/): Portable H.264 and H.265 encoded-video
    activity analysis and bounded IDR replay.
    - `nalsenseactivity`: Detect inexpensive compressed-stream activity hints
      without decoding pixels, while passing every access unit downstream
      unchanged.
    - `nalsensereplay`: Retain a bounded decodable H.264 or H.265 prefix while
      dormant, then replay it and resume live forwarding when activity wakes
      the gate.

  - [`relationmeta`](analytics/relationmeta/): Events, overlays and JSON
    records from analytics relation metadata of any producer.
    - `relationmetaevents`: Emit presence, count, track and classification
      events with time- and frame-based hysteresis.
    - `relationmetaoverlay`: Draw boxes, outlines, relation arrows, labels,
      text and captions in the style of the Cactus web demo.
    - `relationmetatap`: Record each buffer's relation meta as JSON lines.

  - [`usls`](analytics/usls/): Inference elements powered by
    [usls](https://github.com/jamjamjon/usls).
    - [`uslsobjectdetect`](analytics/usls/OBJECT_DETECTION.md): Supports compatible RF-DETR,
      RT-DETR v1/v2/v4, PicoDet, D-FINE, DEIM and DEIMv2 COCO FP32 batch-one
      exports across model sizes with CPU and optional CoreML, preserving the
      original RGB frame.
    - [`uslssegment`](analytics/usls/SEGMENTATION.md): Automatic and prompted
      segmentation across the usls Image Segmentation families, with native
      mask metadata and same-frame detection prompts.

- [`video`](video/)

  - [`wallwarp`](video/wallwarp/): Irregular video wall distribution.
    - `wallwarp`: Map a quadrilateral region of the input onto the whole
      output through a perspective transform, producing the RGBA stream for
      one display. Layouts are described with
      [`wall-geometry`](utils/wall-geometry/).
    - [`glwallwarp`](video/wallwarp/README.md#glwallwarp): The same warp on the
      GPU through OpenGL, taking and giving any raw format in system or GL
      memory.
    - [`walltiles`](video/wallwarp/README.md#walltiles): Every display of a
      layout from one bin, with a source pad per display ID, warping on the
      CPU or the GPU.

## Tools

- [`wallcalibrate`](tools/wallcalibrate/): Photo-based video wall
  calibration. Displays show AprilTag markers, one photo locates them, and
  the result becomes a `wallwarp` layout from the camera's viewpoint or
  rectified onto the wall plane.

- [`wallplay`](tools/wallplay/): Browser playback demo. Displays open a URL
  with their ID, a control page calibrates them from one photo, each display
  plays its `wallwarp` output through `webrtcsink`, and the browsers report
  how far apart they present the same frame.

## Building

The full workspace requires GStreamer 1.28 or newer. NALSense and wallwarp
independently support GStreamer 1.24 (`cargo build -p gst-plugin-nalsense`,
`cargo build -p gst-plugin-wallwarp`). Development requires the GStreamer
headers and pkg-config files, the base runtime plugins, H.264 and H.265
parsers, and the `gst-inspect-1.0` and `gst-launch-1.0` command-line tools.
On Ubuntu, install the corresponding packages:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  ffmpeg \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-libav \
  gstreamer1.0-tools \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev
```

For macOS and other distributions, follow the
[official GStreamer installation guide](https://gstreamer.freedesktop.org/documentation/installing/index.html)
instead of translating these Ubuntu package names. Confirm the development
and runtime tools are discoverable before building:

```sh
pkg-config --modversion gstreamer-1.0
gst-inspect-1.0 --version
```

Install the pinned toolchain and build the workspace:

```sh
mise install
mise run build
```

Inspect any built plugin or element by name:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-inspect-1.0 <plugin-or-element>
```

See each plugin's README for its input requirements, pipeline examples,
configuration, and plugin-specific validation tasks.

## Development

Run the complete local validation gate before submitting a change:

```sh
mise run pre-commit
```

## License

Licensed under the [Apache License 2.0](LICENSE).
