# GStreamer Cactus plugin

The `cactus` plugin runs models on the [Cactus](https://cactuscompute.com)
engine. It provides `cactusmirage`, which describes video frames with
[Mirage](https://cactuscompute.com/blog/mirage), an open-vocabulary vision
model, and writes the result as GStreamer analytics metadata.

Requires GStreamer 1.28 or newer.

## The engine

The engine is linked statically through [`cactus-sys`](../../utils/cactus-sys/),
which fetches prebuilt Apache-2.0 archives pinned by revision and SHA-256 at
build time. This is a deliberate exception to the workspace rule against
opaque prebuilt libraries; nothing prebuilt is committed. See that crate for
the platforms (macOS arm64, Linux arm64 and x86_64) and their C++ runtime
needs.

The engine is one object per process that holds every model, with no
handle, so all its elements live in this plugin and share one lock:

- **One vision model per process.** The first `cactusmirage` to start loads
  its weights; another with different weights (by SHA-256) fails at start,
  even after the first one stopped. There is no unload.
- **Threads are process-global.** The first explicit `threads` before any
  weights load wins; later values are ignored with a warning.
- **Calls are serialized.** Two `cactusmirage` elements, or a later speech
  element, wait for each other.

Weights: `mise run cactus:weights` downloads the pinned `mirage.cact` to
`target/models/mirage.cact`.

## `cactusmirage`

A `BaseTransform` that analyzes each frame in place and passes it on, with
caps `video/x-raw,format=RGB,pixel-aspect-ratio=1/1,interlace-mode=progressive`.
Rows are copied into a packed RGB24 image, because GStreamer pads rows to 4
bytes. Klass `Filter/Analyzer/Video`, rank none.

- **Input size.** Any resolution is analyzed at full size by default: the
  model resizes internally, so downscaling saves no time and hurts text
  reading. `max-analysis-size` caps only the analyzed copy; boxes are always
  in the frame's own pixels. `max-frame-bytes` refuses larger frames.
- **Start.** Loading the weights takes about 15 ms. A dry run on a small
  grey image then turns bad options, a bad language or a malformed `form`
  into a state-change error instead of silently empty frames.
- **`mode=sync` (default): inference blocks the streaming thread** for
  80–650 ms per frame on CPU, depending on options and hardware (tokens cost
  about 0.5 ms each). The element answers LATENCY queries with its
  `latency` property, because that time cannot be derived. For live sources
  put `queue leaky=downstream max-size-buffers=1` before it, which analyzes
  the newest frame and keeps metadata on the frame it describes; for files,
  use `analysis-interval`, or `ptssampler` to drop the frames in between.
  QoS stays off unless enabled.
- **`mode=latest`: video at full rate, results at inference rate.** A worker
  thread owns the engine and analyzes the newest frame, from its own packed
  copy; frames pass at once and add no latency. Each result is attached
  once, to the first buffer after it completes, with an `x-provenance`
  naming the analyzed frame's PTS and running time (the frame record's `pts`
  is the analyzed one too). A flush, a new segment, a stream start or a caps
  change discards the analysis in flight. EOS does not wait for it; stopping
  does, for up to one inference, because the engine call cannot be
  cancelled. Buffer pools keep a relation meta, cleared, on recycled buffers
  (`GST_META_FLAG_POOLED`), which would make frames passed without a result
  look analyzed; the element therefore unpools the meta it attaches, so the
  pool frees it.
- **`analysis-interval`: fewer analyses, full video.** A frame is analyzed
  only when its running time is at least this long after the last analyzed
  frame's; the others pass without a result. Unlike `ptssampler`, no frame
  is dropped. Spacing is start to start, so results come at most once per
  interval. In `mode=sync` the spacing is exact in stream time, and slow
  inference only slows the pipeline. In `mode=latest`, inference slower than
  the interval analyzes back to back, as without one; INFO logs when it
  falls behind and when it catches up. Results then carry `x-provenance` and
  are unpooled, as in `mode=latest`. A flush, a new segment, a stream start
  or a caps change restarts it, as does running time going backwards; frames
  without a timestamp are analyzed whenever possible.
- **`hold-time`** re-attaches the last result to later frames without one,
  for example for `ioutracker`. To keep a result on every frame, set it to
  `analysis-interval` in `mode=sync`, and a little above in `mode=latest`,
  where results arrive one inference late. Keeping old results on screen is
  `relationmetaoverlay`'s job (`expire-overlay`). `hold-time` counts from
  the frame the result was first attached to and needs timestamped buffers
  in a time segment.
- **Errors.** A frame that cannot be copied, described or mapped is a fatal
  stream error, as in `uslsobjectdetect`: after the dry run, failures mean
  a broken engine or a buffer that does not match its caps. In
  `mode=latest` the worker's failure surfaces on the next buffer.

### Output

| Mirage | Metadata |
| --- | --- |
| `objects[]` | top-level `ODMtd`: box in frame pixels, `name` as label, `score` (−1 when not requested) |
| object polygon `mask` | `x-polygon` under the object's `ODMtd`, in 0..1 |
| object `id`, `description`, `position`, `point` | `cactus/mirage/object` under the `ODMtd` |
| `relations[]` | `x-relation` node between the two `ODMtd`s, plus a direct `RELATE_TO` |
| `text[]` | `ODMtd` labelled `text-line` with `x-text` (`string`, `score`) |
| `reasoning`, else `scene` | frame `x-caption`: `reasoning` parts without `lang:`, else `key: value` lines |
| `form` choice and integer fields | one frame-level `ClsMtd` per field, labels `field=value`, including the `-` (no answer) choice; text fields stay in JSON |
| the whole response | frame `cactus/mirage/describe`: `{"engine", "weights", "pts", "response"}` with the engine JSON unchanged |

The `x-*` documents are generic (see
[`analytics-meta`](../../utils/analytics-meta/)): `relationmetaoverlay` and
`relationmetaevents` read them from any producer and never read `cactus/*`
documents. `json-output` controls only the Cactus documents: `full` (both),
`frame` (the record only, which loses the link from each `ODMtd` to its
entry) or `none`. A record over the 1 MiB document cap is dropped with a
warning. `regions` and `events` (always empty so far) appear only in the
record. Items the mapping cannot place (an object without a box, a relation
to an unknown id) are logged at debug level and stay in the record.

Measured documents per frame on a test pattern (an arm64 laptop CPU, release
build): about 1.5 KB with `json-output=none`, 5.3 KB with `frame`, 6 KB with
`full` at 640×360, and 7 KB at 1920×1080; 85–150 ms per frame.

### Properties

| Property | Default | Meaning |
| --- | --- | --- |
| `model-file` | unset (required) | Mirage `.cact` weights |
| `mode` | `sync` | `sync` or `latest` |
| `latency` | 0 | ns added to LATENCY queries in `mode=sync` (at most an hour) |
| `analysis-interval` | 0 (every frame) | least running time in ns between analyzed frames |
| `hold-time` | 0 | ns to re-attach the last result to later frames |
| `sections` | `scene+objects+text+relations+reasoning` | also `regions`, `events` |
| `fields` | `bbox+score+mask+description` | also `point`, `position`; `bbox` is always requested, because an `ODMtd` needs a box |
| `score-threshold`, `text-score-threshold` | 0 (engine default) | engine `conf`, `text_conf` |
| `max-objects`, `max-text-lines`, `max-tokens` | 0 (engine default) | a `max-tokens` that is too small silently empties sections |
| `nms` | `true` | suppress overlapping detections |
| `language` | empty (English) | `de`, `fr`, `es`, `it`, `nl` or `pl` |
| `keywords` | empty | comma-separated words to favour |
| `form` | empty | a JSON object of field schemas, for example `{"person_present":{"enum":["yes","no"]}}` |
| `extra-options` | empty | other engine options as a JSON object; keys with their own property are refused |
| `mask-format` | `polygons` | `largest`, `merged`, or `none` (no masks) |
| `max-analysis-size` | 0 (full size) | longest side of the analyzed copy |
| `max-frame-bytes` | 128 MiB | covers 8K UHD (99.5 MB packed) |
| `json-output` | `full` | `full`, `frame`, `none` |
| `threads` | 0 (engine default) | process-global, see above |

All properties can change only in the NULL and READY states.

### Known limitations

- Object names drift between calls (`framed picture`, then `poster`), so
  label-based events need an allowlist. Names become label quarks, which
  GLib never frees; the set is bounded by the model's vocabulary, times the
  languages used. Zero-shot forms can contradict the
  detections. Object `id`s are only unique within one frame; there is no
  video mode.
- Bitmap masks are not selectable yet: they need one instance
  `SegmentationMtd` per frame through a multi-region bridge.

## Examples

Run from the repository root after `mise run build` and
`mise run cactus:weights`.

Offline at two frames a second:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  filesrc location=in.mp4 \
  ! decodebin3 \
  ! ptssampler period=500000000 \
  ! videoconvert \
  ! video/x-raw,format=RGB \
  ! cactusmirage model-file=target/models/mirage.cact sections=objects+text fields=bbox+score \
  ! videoconvert \
  ! relationmetaoverlay draw-text=true \
  ! videoconvert \
  ! autovideosink
```

A live camera at full rate, with boxes at inference rate (`mode=latest`):

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  autovideosrc \
  ! videoconvert \
  ! video/x-raw,format=RGB \
  ! cactusmirage model-file=target/models/mirage.cact mode=latest \
  ! relationmetaevents labels=person leave-min-time=2000000000 \
  ! videoconvert \
  ! relationmetaoverlay draw-caption=true \
  ! videoconvert \
  ! autovideosink
```

## Development

```sh
cargo test -p gst-plugin-cactus --all-targets
mise run //analytics/cactus:inspect
mise run //analytics/cactus:smoke
```

The default tests use a fake engine behind the element's `Describer` trait;
the smoke tests run the real engine with the pinned weights.
