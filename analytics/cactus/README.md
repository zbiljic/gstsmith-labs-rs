# GStreamer Cactus plugin

The `cactus` plugin runs models on the [Cactus](https://cactuscompute.com)
engine. It provides two elements:

- `cactusmirage` describes video frames with
  [Mirage](https://cactuscompute.com/blog/mirage), an open-vocabulary vision
  model, and writes the result as GStreamer analytics metadata.
- `cactuswhistle` transcribes speech with
  [Whistle](https://cactuscompute.com/blog/whistle) into timed text cues.

Requires GStreamer 1.28 or newer.

## Engines

The engine comes through [`cactus-sys`](../../utils/cactus-sys/), which
fetches prebuilt Apache-2.0 archives pinned by revision and SHA-256 at build
time and links them into a small shared library that the plugin carries
inside itself; nothing else is installed. This is a deliberate exception to
the workspace rule against opaque prebuilt libraries; nothing prebuilt is
committed. See that crate for the platforms (macOS arm64, Linux arm64 and
x86_64) and their C++ runtime needs.

An engine holds one vision and one speech model and runs one call at a
time. The `engine` property picks the engine an element runs on: elements
naming the same engine share it, and each name is a separate engine, loaded
on first use, that runs at the same time as the others. Unset is the
default engine.

- **One vision and one speech model per engine.** The first element of a
  kind to start on an engine loads its weights; another with different
  weights (by SHA-256) on that engine fails at start, even after the first
  one stopped, and needs an engine of its own. Nothing is unloaded: an
  engine and its models stay in memory until the process exits, about
  150 MB with Mirage and 65 MB with Whistle.
- **Elements on one engine take turns,** in the order they asked: a
  300 ms `describe` delays a transcription on the same engine by 300 ms.
- **One stream per engine.** Only one `cactuswhistle` per engine can use
  `mode=stream` at a time; the default `mode=windows` has no such limit.
- **Threads are per engine.** The first explicit `threads` before the
  engine loads weights wins; later values are ignored with a warning.
  Engines running at once share the CPU, so keep their threads together
  within the performance cores. Measured on one machine with eight
  performance cores: two engines at the default threads described 1.6
  times as many frames as one; three at the default threads described
  fewer than one; four at `threads=2` described twice as many.

```sh
# Two cameras described at the same time, speech on the default engine
gst-launch-1.0 \
  v4l2src device=/dev/video0 ! videoconvert ! cactusmirage model-file=mirage.cact engine=cam0 mode=latest ! fakesink \
  v4l2src device=/dev/video2 ! videoconvert ! cactusmirage model-file=mirage.cact engine=cam1 mode=latest ! fakesink \
  autoaudiosrc ! audioconvert ! audioresample ! queue ! cactuswhistle model-file=whistle.cact ! fakesink
```

Weights: `mise run cactus:weights` downloads the pinned `mirage.cact` and
`whistle.cact` to `target/models/`.

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
| `engine` | unset (default engine) | engine name, see [Engines](#engines) |
| `threads` | 0 (engine default) | per engine, see [Engines](#engines) |

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

## `cactuswhistle`

Speech to text, with the pads of `whispertranscriber`: 16 kHz mono F32LE in
(put `audioconvert ! audioresample` before it), `text/x-raw,format=utf8`
cues out. Klass `Text/Audio/Filter`, long name `Transcriber`, rank none.
Whistle understands English, German, French, Spanish, Italian, Dutch and
Polish, and detects the language unless `language` is set; a tag such as
`en-US` counts as its language.

- **`mode=windows` (default): rolling windows.** Once `chunk-duration` of
  audio past the commit point has arrived, the element transcribes a window
  made of up to `context-duration` of already committed audio and that new
  audio. Each call is stateless, so any number of elements can share the
  engine. Words ending within `holdback` of the window's end wait for the
  next window, which hears them with the audio that follows (like
  `whispertranscriber`'s `live-edge-offset`); the commit point therefore
  trails each window by the holdback, and windows run every
  `chunk-duration − holdback` of input: with the defaults, a 12 s window
  every second, 10–30% of a laptop CPU core. Words also wait until two
  consecutive windows transcribe them alike: with the default `agreement`
  of 2000, the chunk length, every new word does, which drops phrases a
  single window invents in noise and costs no accuracy on clean speech.
  Keep `agreement` at or above `chunk-duration` for that; a smaller one
  checks only the words that end within it of the commit point, and 0
  none. The input's last window commits its words as heard, and so does a
  window that has waited two chunks and the holdback past the commit point
  for two windows to agree. Context words that repeat committed ones are
  dropped, so each word is committed once.
- **`mode=stream`: the engine's streaming transcriber.** Each
  `chunk-duration` step goes to the engine, which commits the words two
  consecutive passes agree on and keeps an unconfirmed tail. Every word the
  engine commits is kept; it can commit a word a step or two after a step
  with nothing pending, so silence counts as settled only a step and a
  second behind the audio. An engine has one stream, so a second element
  in this mode on the same engine fails at start.
- **`chunk-duration=0` (`mode=windows`): the whole recording at EOS**, up
  to an hour; the engine windows audio longer than 30 seconds itself.
- **Cues.** Committed words are grouped into cues that break at a pause
  over 1 s, after 7 s or 84 characters, or at sentence punctuation once the
  cue lasts 1.2 s. A cue stays open across windows until one of those
  happens, so sentences are not cut at window boundaries. Cue PTS and
  durations come from Whistle's word times (80 ms steps), on the input
  timeline. GAP events cover the time before the open cue, so downstream
  elements keep moving through silence; the first one, empty, marks the
  start, so sinks preroll before the first cue.
- **Invented phrases.** In static, Whistle can hear a short phrase such as
  "Thank you." and time it to a step or two. Cues of two or more words
  spoken faster than `max-word-rate` (10 words a second; speech rarely
  passes 6) are dropped, in both modes, except from a window that commits
  as heard (above), whose last words can be timed to zero length.
- **Metadata.** A `cactuswhistle` element message per window or step
  carries `window-start`, `window-end` (seconds), `transcription-seconds`,
  `words`, `committed`, `cues`, `dropped` (cues spoken too fast),
  `forced` (the window committed as heard),
  `language` and `pending`, the engine's
  unconfirmed text in `mode=stream` (empty otherwise). A change in the
  detected language is sent downstream as a `language-code` tag.
- **Timeline.** Gaps in the input (GAP events or missing time) are filled
  with silence, which Whistle transcribes in about 0.1 ms per second. A
  flush, a new segment or a stream start drops audio not yet transcribed
  and starts over.
- **Latency.** Transcription runs on the streaming thread; for live sources
  put a `queue` before the element. A cue's PTS is its first word, and the
  cue stays open for up to 8 s (7 s of words, then a pause), so a cue can
  leave the element that long after its first word, plus the time its last
  word takes to be committed: a chunk, or with `agreement` up to two
  chunks and the holdback (a window that commits as heard), or in
  `mode=stream` two steps and a second. `latency=0` reports the sum, plus
  2 s for transcription and for waiting while another element uses the
  engine: 15 s with the defaults. Short cues leave much sooner;
  with a smaller `latency`, a syncing sink may get late cues. In
  `chunk-duration=0` mode it reports 0.

Measured on an arm64 laptop CPU: transcribing a 12 s window takes 0.1–0.3 s,
30 s about 0.75 s. On a 96 s synthetic three-voice recording with a known
script, the word error rates were:

| Setting | Word error rate |
| --- | --- |
| `chunk-duration=0` (whole recording) | 5.5% |
| default (`windows`, 2 s chunks, 10 s context, 1 s holdback, 2 s agreement) | 5.1% |
| `windows` without agreement | 5.1% |
| `windows`, 5 s context, no agreement | 7.1% |
| `windows`, 5 s chunks, 2 s holdback, no agreement | 5.9% |
| `mode=stream`, 2 s steps | 6.3% |
| `mode=stream`, 1 s steps | 6.7% |
| `mode=stream`, 0.5 s steps | 5.9% |

Short windows lose words: the model needs a few seconds around a word.
These are single runs on one recording, not benchmarks.

### Properties

| Property | Default | Meaning |
| --- | --- | --- |
| `model-file` | unset (required) | Whistle `.cact` weights |
| `mode` | `windows` | `windows` or `stream` |
| `language` | empty (detect) | `en`, `de`, `fr`, `es`, `it`, `nl` or `pl`, or a tag such as `en-US` (read back as set) |
| `language-code` | empty (detect) | the same setting, under the name `transcriberbin` sets |
| `keywords` | empty | comma-separated words to favour, such as names |
| `chunk-duration` | 2000 | ms of new audio per window or stream step; 0 transcribes at EOS (`windows` only) |
| `context-duration` | 10000 | ms of committed audio repeated before each window (`windows`) |
| `holdback` | 1000 | ms before a window's end where words wait for the next window (`windows`) |
| `agreement` | 2000 | ms before the commit point where words wait for a second, identical transcription (`windows`) |
| `max-word-rate` | 10 | words per second above which a cue of two or more words is dropped; 0 keeps every cue |
| `latency` | 0 (automatic) | ms added to LATENCY queries, as in `whispertranscriber` (`cactusmirage` uses ns) |
| `engine` | unset (default engine) | engine name, see [Engines](#engines) |
| `threads` | 0 (engine default) | per engine, see [Engines](#engines) |

All properties can change only in the NULL and READY states.

### In `transcriberbin`

`cactuswhistle` can be the `transcriber` of `transcriberbin` (set on its
`sink_audio` pad), which turns the cues into closed captions on the video.
`transcriberbin` sets the element's `latency` to its own (4 s by default),
and its `language-code` when the pad's changes, restarting the transcriber
so that it takes effect. Cues that come later than
that latency are placed on later frames rather than dropped; a
`transcriberbin` latency of 15 s keeps the captions on time.

### Known limitations

- Noisy audio makes Whistle repeat phrases and invent words. Agreement and
  `max-word-rate` drop the short phrases it invents in static, not longer
  ones. On such audio, rolling windows, the engine's stream and the whole
  recording each make different mistakes.
- There is no speaker attribution and no translation.

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

Subtitles for a recording:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 -q \
  filesrc location=in.wav \
  ! decodebin3 \
  ! audioconvert \
  ! audioresample \
  ! cactuswhistle model-file=target/models/whistle.cact \
  ! srtenc \
  ! fdsink > in.srt
```

## Development

```sh
cargo test -p gst-plugin-cactus --all-targets
mise run //analytics/cactus:inspect
mise run //analytics/cactus:smoke
```

The default tests use a fake engine behind each element's `Describer` or
`Transcriber` trait; the smoke tests run the real engine with the pinned
weights and the public-domain Apollo 13 "Houston, we've had a problem"
recording, which `mise run cactus:weights` also downloads.
