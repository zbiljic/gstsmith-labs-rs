# GStreamer video wall warp plugin

The `wallwarp` plugin provides `wallwarp`, which produces the stream for one
display of an irregular video wall. It maps a quadrilateral region of the
input frame onto the whole output frame through a perspective transform, so a
display can be cropped, scaled, rotated, or keystoned onto the shared source.
Run one instance per display behind a `tee`.

The element is experimental. It requires GStreamer 1.24 or newer, and needs
only the core, base, and video libraries. Layout files and the geometry behind
them live in [`wall-geometry`](../../utils/wall-geometry/), which does not
depend on GStreamer.

## Behavior

For every output pixel, `wallwarp` takes its center in normalized display
coordinates `(u, v)` in `[0, 1]²`, maps it through the homography from the unit
square to `quad`, and samples the input at the resulting normalized source
position. Output pixels whose sample falls outside the input frame are filled
with `background`.

`quad` lists the normalized source positions of the output's top-left,
top-right, bottom-right, and bottom-left corners, in that order. `0,0` is the
input's top-left and `1,1` its bottom-right; values outside `[0, 1]` reach past
the input edge. The corners must form a strictly convex quadrilateral. Listing
them counter-clockwise mirrors the image, which suits rear projection. Some
quads in the same notation:

| `quad` | Output |
|---|---|
| `0,0,1,0,1,1,0,1` | The whole input (default) |
| `0.25,0.25,0.75,0.25,0.75,0.75,0.25,0.75` | The central quarter |
| `1,0,1,1,0,1,0,0` | Rotated for a display mounted 90° clockwise |
| `1,1,0,1,0,0,1,0` | Rotated by 180° |
| `1,0,0,0,0,1,1,1` | Mirrored horizontally |
| `0.25,0,0.75,0,1,1,0,1` | A trapezoid, sampled with perspective |

The default identity quad, with equal input and output video info, runs in
passthrough mode without copying.

### Caps

Both pads accept `video/x-raw, format=RGBA` in system memory. The output size
comes from `output-width` and `output-height`. A dimension left at `0` is
negotiated with downstream, preferring the input dimension. The frame rate
passes through, and output pixels are square unless downstream requires
otherwise; the input pixel aspect ratio does not affect sampling.

The output size can change while streaming. The next frame renegotiates, and
downstream receives new caps before it; passing through an identity starts or
stops as the sizes come to match or differ. Setting `output-width` and
`output-height` one after the other can leave a frame in between with one
dimension changed. To change both in one step, leave them at `0` and change
the `caps` of a `capsfilter` after the element, with
`caps-change-mode=delayed`: by default it refuses a frame of the old size that
is already on its way, which stops the stream.

Timestamps, durations, offsets, and buffer flags (except `gap`) are copied to
each output buffer. Segments, flushes, and EOS pass through unchanged. Metas
without tags, or tagged only `video`, are copied; metas tied to frame size,
orientation, or memory layout, such as video metas and regions of interest,
are dropped.

## Properties

Every property can change while streaming, and the next frame after a change
uses it. An invalid `quad` is ignored with a warning, keeping the current one.

| Property | Default | Meaning |
|---|---|---|
| `quad` | `0,0,1,0,1,1,0,1` | Eight normalized source coordinates: `x,y` of the output's top-left, top-right, bottom-right, and bottom-left corners, separated by commas and/or spaces |
| `output-width` | `0` | Output width in pixels; `0` negotiates it, preferring the input width |
| `output-height` | `0` | Output height in pixels; `0` negotiates it, preferring the input height |
| `interpolation` | `bilinear` | `nearest` copies the closest source pixel; `bilinear` blends the four closest |
| `background` | `0xff000000` | Color of output pixels sampling outside the input, as big-endian ARGB (`0xAARRGGBB`) |
| `n-threads` | `1` | Maximum number of threads warping each frame; `0` uses one per CPU core |

An invalid `quad`, with the wrong number of values, non-finite values, or
repeated, collinear, concave, or self-intersecting corners, is logged when it
is set. The element then refuses to leave READY and posts a
`GST_LIBRARY_ERROR_SETTINGS` error naming the problem.

## Examples

Run from the repository root after `mise run build`. Show the upper-left
region of a test pattern, keystoned, at 1280x720:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  videotestsrc \
      num-buffers=60 \
  ! video/x-raw,format=RGBA,width=1920,height=1080 \
  ! wallwarp \
      quad="0.04,0.09 0.41,0.08 0.42,0.46 0.05,0.48" \
      output-width=1280 \
      output-height=720 \
  ! videoconvert \
  ! autovideosink
```

### Three-display wall

[`examples/three-screens.json`](examples/three-screens.json) describes three
displays on a 1920x1080 canvas. They have different resolutions and gaps
between them, part of the canvas is covered by no display, and the screens are
listed in no spatial order. `display-a` and `display-c` are slightly keystoned.
`display-b` is a 1920x1080 panel mounted rotated 90° clockwise, so its own
top-left corner is near the canvas top-right. See `wall-geometry` for the
layout format, and [`wallcalibrate`](../../tools/wallcalibrate/) to generate a
layout from a photo of real displays.

The `wall-demo` example reads a layout and builds one
`tee ! queue ! wallwarp` branch per screen from it, so the geometry exists only
in the JSON file. By default it renders one frame and writes
`screen-<id>.png` for each screen, plus `source.png`; `pngenc` comes from
gst-plugins-good:

```sh
cargo run --release -p gst-plugin-wallwarp --example wall-demo -- \
  video/wallwarp/examples/three-screens.json --output-dir wall-demo
```

`--source-size WIDTHxHEIGHT` renders a source of a different resolution; it
must keep the canvas aspect ratio. `--pattern` picks the `videotestsrc`
pattern. `--html` also writes `screen-<id>.html`, a page that stretches each
PNG over a whole display like the calibration patterns, and an `index.html`
linking them, for checking a calibrated layout on real displays.

With `--print-launch` the example prints the equivalent live
`gst-launch-1.0` command, with one window per screen. Use a release build;
unoptimized builds are too slow for real-time playback:

```sh
cargo build --release -p gst-plugin-wallwarp
cargo run --release -p gst-plugin-wallwarp --example wall-demo -- --print-launch
```

The printed pipeline has this shape, with one branch per screen:

```sh
GST_PLUGIN_PATH="$PWD/target/release" gst-launch-1.0 \
  videotestsrc \
      pattern=smpte \
      is-live=true \
  ! video/x-raw,format=RGBA,width=1920,height=1080 \
  ! tee \
      name=wall \
  wall. \
  ! queue \
  ! wallwarp \
      name=display-c \
      quad="0.18229166666666666,0.6481481481481481,0.5,0.6111111111111112,0.5052083333333334,0.9259259259259259,0.1875,0.9537037037037037" \
      output-width=1024 \
      output-height=576 \
  ! videoconvert \
  ! autovideosink \
  wall. \
  ! queue \
  ! wallwarp \
      name=display-a \
      ...
```

To show a video file with the canvas aspect ratio instead, replace the
`videotestsrc` lines with a decoder and converters, for example
`filesrc location=movie.mp4 ! decodebin ! videoconvert ! videoscale`, ahead of
the caps filter.

## Design notes

- `wallwarp` subclasses `GstVideoFilter` in never-in-place mode, like
  `videoscale` and `videoflip`. The base class parses video info, sizes output
  buffers from the output caps, proposes and decides video-meta buffer pools,
  maps frames with their real strides and offsets, and filters
  geometry-dependent metas. The element implements caps transformation and
  fixation, `set_info` to locate samples, and `transform_frame`.
- The homography is Heckbert's closed-form unit-square-to-quadrilateral map,
  composed with the input and output pixel scales. It is exact for crops and
  quarter turns. Invalid or ill-conditioned transforms are rejected before
  streaming.
- Sampling is safe Rust over strided byte planes, without `unsafe` or
  intrinsics, written so the compiler vectorizes it. Nearest truncates
  pixel-center coordinates. Bilinear uses 8-bit fixed-point weights and
  clamps to the edge inside the source; samples outside it use the
  background. A unit test compares every pixel with a direct per-pixel
  transcription of these rules.
- A new `quad` or frame size locates every output pixel's sample once, in
  `f64`, and keeps it as a 16-bit column and row plus, for bilinear, two 8-bit
  weights: 4 or 6 bytes per output pixel, about 12 MB for a bilinear
  1920x1080 output. Bilinear clamps through its weights: past the last pixel
  center, all the weight goes to the last pixel. Each frame then only reads
  the source pixels around 64 samples at a time and blends them, without
  allocating or parsing.
- With `n-threads` above 1, threads started for each frame join the streaming
  thread in taking bands of output rows, four bands per thread, from a shared
  queue.

## Performance

Per output frame from a 1920x1080 source, on a 3.2 GHz arm64 CPU, as
measured by the `hot_path` benchmark:

| Output | Nearest | Bilinear | Bilinear, 4 threads |
|---|---:|---:|---:|
| 1280x720, perspective | 0.6 ms | 2.6 ms | 0.7 ms |
| 1920x1080, rotated | 1.6 ms | 6.3 ms | 1.7 ms |

Locating the samples again for a new `quad` or frame size takes about 3 ms
for a bilinear 1280x720 output and 8 ms for 1920x1080, on the thread that
sets the `quad` or negotiates the caps.

## Limitations

- RGBA in system memory only; convert other formats with `videoconvert`.
  Alpha is interpolated without premultiplication. Interlaced input is
  processed as progressive frames.
- Sampling runs on the CPU, with no GPU path yet. Each `tee` branch runs in
  its own `queue` thread, and `n-threads` adds threads within a branch.
- Sources wider than 65534 or taller than 65535 pixels are not supported.
- Bilinear sampling does not prefilter, so outputs much smaller than their
  source region alias.
- The element knows nothing about canvases. Layout tooling must give it a
  source with the canvas aspect ratio, as `wall-demo` checks.

## Development

```sh
cargo check -p gst-plugin-wallwarp --all-targets
cargo test -p wall-geometry -p gst-plugin-wallwarp --all-targets
cargo clippy -p wall-geometry -p gst-plugin-wallwarp --all-targets -- -D warnings
```

The tests cover the identity, crops, quarter turns, perspective trapezoids,
background fill, invalid quads, padded and offset input rows, odd sizes, caps
negotiation, changes of every property while streaming, timestamps, flags,
flushing, EOS, and the three-screen layout
through a `tee` with independent branches. Every pixel of several warps is
compared with a direct per-pixel sampler, on one thread and on several.

Benchmark perspective and rotated outputs from a 1080p source, on one thread
and on four, plus identity passthrough:

```sh
cargo bench --locked -p gst-plugin-wallwarp --bench hot_path
```
