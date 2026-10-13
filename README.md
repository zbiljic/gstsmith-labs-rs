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

  - [`nalsense`](analytics/nalsense/): Portable H.264 and H.265 encoded-video
    activity analysis and bounded IDR replay.
    - `nalsenseactivity`: Detect inexpensive compressed-stream activity hints
      without decoding pixels, while passing every access unit downstream
      unchanged.
    - `nalsensereplay`: Retain a bounded decodable H.264 or H.265 prefix while
      dormant, then replay it and resume live forwarding when activity wakes
      the gate.

  - [`usls`](analytics/usls/): Inference elements powered by
    [usls](https://github.com/jamjamjon/usls).
    - `uslsobjectdetect`: The first element, supporting compatible RF-DETR,
      RT-DETR v1/v2/v4, PicoDet, D-FINE, DEIM and DEIMv2 COCO FP32 batch-one
      exports across model sizes with CPU and optional CoreML, preserving the
      original RGB frame.

## Building

The full workspace requires GStreamer 1.28 or newer. NALSense independently
supports GStreamer 1.24 (`cargo build -p gst-plugin-nalsense`). Development requires
the GStreamer headers and pkg-config files, the base runtime plugins, H.264 and
H.265 parsers, and the `gst-inspect-1.0` and `gst-launch-1.0` command-line tools.
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
