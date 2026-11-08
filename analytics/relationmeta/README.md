# GStreamer relation meta plugin

The `relationmeta` plugin turns GStreamer analytics relation metadata from
any producer into events, overlays and JSON records. `relationmetaevents`
and `relationmetaoverlay` read only standard Mtds (`ODMtd`, `ClsMtd`,
`TrackingMtd`, `SegmentationMtd`) and the generic `x-*` documents of
[`analytics-meta`](../../utils/analytics-meta/): `x-polygon`, `x-relation`,
`x-caption`, `x-text` and `x-provenance`. They never read producer-specific
documents, so they behave the same on `uslsobjectdetect`, `ioutracker`,
`onvifmeta2relationmeta` and any later producer. `relationmetatap` records
everything, producer-specific documents included.

Requires GStreamer 1.28 or newer. Before 1.28.3, copying a buffer drops its
boxes and everything linked to them, so a frame that is shared when it
reaches the overlay (after a `tee`, for example) is drawn without them.
`relationmetaoverlay` also needs pango and cairo.

## `relationmetaevents`

A passthrough element that emits events when what the analytics see changes.
Buffers and metadata pass through unchanged; it accepts any caps.

- **Labels.** `present`, `absent` and `count-changed` per `ODMtd` label.
  This is the only option for producers without tracking.
- **Tracks.** With `TrackingMtd`, `entered` and `left` per track id, with
  the label of the related `ODMtd`. Lost tracks count as gone.
- **Classes.** `class-changed` for frame-level `ClsMtd`s (ones related to no
  other Mtd), keyed by their sorted label set until GStreamer 1.30 adds
  semantic tags. The label allowlist and `min-score` do not apply to them.
  Zero-shot classifications can be unreliable; treat these as hints.
- **Observations.** Only buffers that carry a relation meta are analyzed
  frames. A buffer without one is skipped, so a producer that analyzes every
  few frames does not cause flapping absences. An empty relation meta is an
  analyzed frame that saw nothing. A note for producers: buffer pools keep a
  relation meta, cleared, on recycled buffers (`GST_META_FLAG_POOLED`), so a
  producer that attaches results to only some frames should unpool its meta
  (`analytics_meta::unpool`, as `cactusmirage` does in `mode=latest` or
  with `analysis-interval`), or its
  skipped frames read as "saw nothing". A result attached again to later frames
  (same `x-provenance` PTS as the previous one) is one analysis: time
  advances, but it does not count as another frame.
- **Hysteresis.** A change must persist for `*-min-frames` consecutive
  analyzed frames *and* `*-min-time` of running time. Time works under
  variable inference rates; without timestamps only frames count. With a
  producer that analyzes at most every N ms (`cactusmirage
  analysis-interval`), each of the `*-min-frames` lasts about N ms.
- **Label churn.** Open-vocabulary detectors drift between names (`poster`
  and `framed picture` for the same box). Use `labels` and `min-score` to
  keep presence from flapping.
- **Reset.** Stream start and a flush forget all state without emitting
  events. A new segment keeps what is present but restarts time-based
  hysteresis, because running time may jump. EOS emits nothing.
- **Classifications** are remembered per label set for the stream's life;
  producers use a fixed set of fields per run.

Each event is a `relationmeta-event` structure, posted as an element message
and pushed downstream as a serialized custom event ahead of the buffer that
caused it, so `jsontap` records it in stream order.

| Field | Type | Present |
| --- | --- | --- |
| `type` | string | always: `present`, `absent`, `count-changed`, `entered`, `left`, `class-changed` |
| `stream-id` | string | always |
| `label` | string | except tracks without a related `ODMtd` |
| `track-id` | u64 | `entered`, `left` |
| `count` | u32 | label events (0 for `absent`) |
| `score` | double | `present`, `count-changed`, `entered`, `class-changed` |
| `pts`, `running-time` | u64 ns | when the buffer has them |
| `analyzed-pts` | u64 ns | when an `x-provenance` names the analyzed frame |

| Property | Default | Meaning |
| --- | --- | --- |
| `stream-id` | `stream` | copied into every event |
| `labels` | empty | comma-separated allowlist; empty reports every label |
| `min-score` | 0 | objects scoring below this are ignored |
| `enter-min-frames`, `leave-min-frames` | 1 | consecutive analyzed frames before a change in / a disappearance |
| `enter-min-time`, `leave-min-time` | 0 | running time in ns the change must also persist |
| `events` | all | flags: `present+absent+count-changed+entered+left+class-changed` |

## `relationmetaoverlay`

Draws what the metadata describes, in the style of the Cactus web demo:

1. polygons (`x-polygon`) and segmentation masks: class colour at 10% fill
   (`polygon-opacity`) with a 35% outline, or the fill's opacity if higher;
   masks at 20% (`mask-opacity`), without outline;
2. boxes: a 1 px outline in the class colour at 75%;
3. relations (`x-relation`): a `#fafafa` line at 75% from the subject's box
   centre to the object's, with an open arrowhead, and the predicate
   (`_` shown as a space) in a chip at the midpoint;
4. label chips: `name | 69%` in monospace on `#0a0a0a` at 90%, with a border
   in the class colour, above the box's top-left corner or right-aligned
   when the box starts in the right half, clamped inside the frame, and
   stepped up and down in label-height steps to avoid other chips. Tracked
   objects show their id: `man #3 | 69%`;
5. optionally, recognized text (`x-text`) in place of `text-line` labels,
   and the frame caption (`x-caption`) at the bottom left.

Colours come from a hash of the label over six pastels (`#7dd3e8`,
`#b6d98c`, `#f2c879`, `#c4a5f5`, `#f1a8bd`, `#f5aa91`). The aim is the same
look as the demo, not pixel equality: fonts differ, and the demo hashes
UTF-16 units while this hashes chars.

When downstream accepts `GstVideoOverlayCompositionMeta` in its allocation
query, asked on the first buffer after caps, the overlay is attached as that
meta (merged with an existing one); otherwise it is blended into the frame.
The rendered overlay is cached: frames without relation meta and results
identical to the last one reuse it. Each new result renders a full-frame
ARGB surface, which costs more at 4K than at 640×480. Like upstream overlays,
the element needs writable frames, so a shared input buffer is copied.

| Property | Default | Meaning |
| --- | --- | --- |
| `draw-masks`, `draw-boxes`, `draw-relations`, `draw-labels` | `true` | layers to draw |
| `draw-text`, `draw-caption` | `false` | recognized text and the caption |
| `font-desc` | `monospace 14` | pango font of all chips; sizes are pixels |
| `line-width` | 1 | outline, arrow and border width in pixels |
| `polygon-opacity`, `mask-opacity` | 0.1, 0.2 | fill opacity of polygons and of segmentation masks |
| `palette` | `pastel` | `pastel` (by label), `tracking` (by track id), `mono` |
| `expire-overlay` | 1 s | how long the last result stays on frames without relation meta; 0 draws only frames that carry it. Keep it at or above the producer's analysis interval |

Formats: exactly the formats `gst_video_blend` handles
(`GST_VIDEO_OVERLAY_COMPOSITION_BLEND_FORMATS`): BGRx, RGBx, xRGB, xBGR, RGBA,
BGRA, ARGB, ABGR, RGB, BGR, I420, YV12, AYUV, YUY2, UYVY, v308, Y41B, Y42B,
Y444, NV12, NV21, A420, YUV9, YVU9, IYU1 and GRAY8.

`x-provenance` is not drawn.

## `relationmetatap`

A passthrough development tool that writes one JSON line per buffer to
`location` (truncated on start, flushed at EOS and stop):

```json
{"pts":40000000,"running-time":40000000,"mtds":[{"id":0,"type":"object-detection","label":"man","x":10,"y":10,"w":50,"h":80,"r":0.0,"confidence":0.7}]}
```

`mtds` lists every Mtd with its `id`, its `type` (upstream's name; unknown
types are recorded by name only), its type-specific fields (box, label and
confidence; classes; tracking info; segmentation location and region ids,
without mask pixels), its outgoing `relations` when it has any, and JSON
documents (`x-json`) with their `schema` and parsed `json`, or the raw
`text` of a document that is not valid JSON. Documents are re-serialized, so
keys are sorted and numbers normalized, but no value is lost. `mtds` is
`null` on a buffer without relation meta and `[]` on one with an empty
relation meta. Unlike the other elements, it records producer-specific
documents too. Writes block the streaming thread.

| Property | Default | Meaning |
| --- | --- | --- |
| `location` | required | path of the JSON lines file |

## Example

Run from the repository root after `mise run build`, with an object
detector that writes relation metadata:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  filesrc location=in.mp4 \
  ! decodebin3 \
  ! videoconvert \
  ! uslsobjectdetect model-file=model.onnx \
  ! relationmetaevents labels=person leave-min-time=1000000000 \
  ! videoconvert \
  ! relationmetaoverlay \
  ! videoconvert \
  ! autovideosink
```

## Development

```sh
cargo test -p gst-plugin-relationmeta --all-targets
cargo clippy -p gst-plugin-relationmeta --all-targets -- -D warnings
mise run //analytics/relationmeta:inspect
```

Render a demo-style frame to look at the style by eye:

```sh
RELATIONMETA_DEMO_PNG=/tmp/demo.png \
  cargo test -p gst-plugin-relationmeta --test relationmetaoverlay demo -- --ignored
```
