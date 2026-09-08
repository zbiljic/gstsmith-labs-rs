# gstsmith-labs-rs

Repository containing experimental
[GStreamer](https://gstreamer.freedesktop.org/) plugins written in Rust.
Plugins remain here while their behavior, portability, and operational limits
are being established before promotion to
[`gstsmith-rs`](https://github.com/zbiljic/gstsmith-rs).

## Plugins

- [`analytics`](analytics/)

  - [`nalsense`](analytics/nalsense/): Portable H.264 encoded-video activity
    analysis and bounded IDR replay.
    - `nalsenseactivity`: Detect inexpensive compressed-stream activity hints
      without decoding pixels, while passing every access unit downstream
      unchanged.
    - `nalsensereplay`: Retain a bounded decodable H.264 prefix while dormant,
      then replay it and resume live forwarding when activity wakes the gate.

## Building

The workspace requires GStreamer 1.24 or newer. Development requires the
GStreamer headers and pkg-config files, the base runtime plugins, an H.264
parser, and the `gst-inspect-1.0` and `gst-launch-1.0` command-line tools. On
Ubuntu, install the corresponding packages:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  ffmpeg \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-libav \
  gstreamer1.0-tools \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev
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
