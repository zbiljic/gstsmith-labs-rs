# NALSense

The `nalsense` plugin provides inexpensive activity hints and bounded replay
for parsed H.264 and H.265 streams. It operates on compressed access units and
does not decode pixels.

The plugin contains two elements:

- `nalsenseactivity` passes H.264 or H.265 access units through unchanged and
  reports encoded-size activity transitions.
- `nalsensereplay` retains a bounded prefix from the latest IDR while dormant,
  then replays that prefix before forwarding live data after a wake.

The plugin requires GStreamer 1.24 or newer.

## Input contract

Both elements use the same always-present sink and source caps. Either of
these structures is accepted:

```text
video/x-h264,
    parsed=(boolean)true,
    stream-format=(string)byte-stream,
    alignment=(string)au

video/x-h265,
    parsed=(boolean)true,
    stream-format=(string)byte-stream,
    alignment=(string)au
```

Place `h264parse` or `h265parse` before the plugin to assemble access units and
convert the stream to Annex B framing. For bounded replay, use
`config-interval=-1` so parameter sets accompany each IDR and are retained in
the replay prefix.

## Activity analysis

This pipeline prints element messages while preserving every compressed
H.264 buffer:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" \
gst-launch-1.0 \
  -m \
  filesrc location=input.h264 \
  ! h264parse \
  ! 'video/x-h264,parsed=(boolean)true,stream-format=(string)byte-stream,alignment=(string)au' \
  ! nalsenseactivity stream-id=camera-1 \
  ! fakesink
```

The equivalent H.265 pipeline is:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" \
gst-launch-1.0 \
  -m \
  filesrc location=input.h265 \
  ! h265parse \
  ! 'video/x-h265,parsed=(boolean)true,stream-format=(string)byte-stream,alignment=(string)au' \
  ! nalsenseactivity stream-id=camera-1 \
  ! fakesink
```

`nalsenseactivity` classifies I, P, and B pictures from slice headers. It sums
VCL payload bytes within each access unit, keeps independent exponentially
weighted baselines for P and B pictures, and excludes keyframes from activity
transitions. Consecutive anomalous frames start activity; consecutive normal
frames clear it.

For H.265, the scanner reads only the bounded prefix needed from the PPS and
first independent slice header. It supports base-layer HEVC VCL NAL types and
does not decode the picture.

| Property | Default | Purpose |
| --- | ---: | --- |
| `stream-id` | `stream` | Identifier included in event messages |
| `activity-threshold` | `3.0` | Minimum pre-update standard-deviation score |
| `baseline-alpha` | `0.02` | Baseline update weight in `(0, 1]` |
| `activity-min-frames` | `3` | Consecutive anomalies required to start |
| `activity-clear-frames` | `4` | Consecutive normal frames required to stop |
| `warmup-frames` | `30` | Observations learned before transitions are eligible |
| `post-idr-guard-frames` | `0` | Post-IDR frames that learn but cannot change state |

These are generic defaults shared by H.264 and H.265. The element selects its
access-unit scanner from negotiated caps, but it does not select an activity
profile by codec. Applications should calibrate the activity properties for
each camera encode profile and set them explicitly when the generic defaults
are not appropriate.

### Recommended starting settings

Use these as initial configurations, then validate them against representative
recordings from the actual camera encode profile:

| Encode profile | Threshold | Alpha | Start frames | Clear frames | Warmup frames |
| --- | ---: | ---: | ---: | ---: | ---: |
| Generic / H.264 | `3.0` | `0.02` | `3` | `4` | `30` |
| x265 CRF camera encode | `2.70` | `0.02` | `3` | `4` | `30` |

For the H.265 starting profile:

```text
nalsenseactivity \
    activity-threshold=2.70 \
    baseline-alpha=0.02 \
    activity-min-frames=3 \
    activity-clear-frames=4 \
    warmup-frames=30
```

The H.265 recommendation is not selected automatically and is not expected to
fit every HEVC encoder or rate-control mode. Keep the generic element default
when the encode profile has not been calibrated.

Each transition is posted as a `nalsense-activity` bus message and sent as a
serialized downstream custom event. Its fields are:

- `type`: `activity-start` or `activity-stop`;
- `stream-id` and zero-based `frame-number`;
- `picture-type` and `reference-picture`;
- `score`, `intensity`, `frame-size`, and `baseline-size`;
- `timestamp-us`, when the input buffer has PTS or DTS.

Stream-start, segment, flush-stop, caps renegotiation, and discontinuous input
reset the analyzer before more observations are accepted.

## Bounded replay

`nalsensereplay` is intended immediately downstream of `nalsenseactivity`:

```text
h264parse config-interval=-1 ! nalsenseactivity ! nalsensereplay ! decoder
h265parse config-interval=-1 ! nalsenseactivity ! nalsensereplay ! decoder
```

While dormant, it retains complete access units from the most recent IDR. An
`activity-start` event wakes it, causing the retained prefix to be emitted
before the access unit that triggered the event. The first replayed buffer is
marked discontinuous so a downstream decoder can establish a fresh decoding
region.

An application may also invoke the `wake` and `sleep` action signals. `wake`
uses a retained prefix when one is available, or waits for the next IDR.
`sleep` continues forwarding until the next IDR, then makes that IDR the start
of a new dormant prefix. The element does not choose a sleep policy itself.

| Property | Default | Purpose |
| --- | ---: | --- |
| `max-buffer-frames` | `300` | Maximum retained access units |
| `max-buffer-bytes` | `16777216` | Maximum retained compressed bytes |
| `start-active` | `false` | Forward immediately at the start of a run |
| `active` | read-only | Whether buffers are currently forwarded |
| `buffered-frames` / `buffered-bytes` | read-only | Current retained prefix |
| `replayed-frames` / `replayed-bytes` | read-only | Cumulative replay totals for the run |
| `forced-wakes` | read-only | Wakes required to preserve configured bounds |
| `peak-buffered-frames` / `peak-buffered-bytes` | read-only | Peak retained prefix for the run |

If retaining another access unit would exceed either bound, replay wakes
instead of keeping an undecodable partial prefix or allowing memory growth.
Stream changes and discontinuities discard the retained prefix and reset codec
scanner state. Active and pending-wake requests survive that invalidation; a
pending sleep becomes dormant because decoding continuity has already ended.
Counters remain cumulative until a new PAUSED run begins.

Each wake posts a `nalsense-replay` message containing `type=wake`, `reason`,
`replayed-frames`, and `replayed-bytes`.

## Optional diagnostics

For H.264, the `qp-diagnostics` Cargo feature parses SPS/PPS and slice headers
to include the luma QP in TRACE logging. H.265 reports `qp=na`. The feature does
not change activity scoring or the public event schema.

## Limitations

Encoded-size activity is a global stream heuristic, not spatial object
detection. Rate-control changes, scene cuts, camera movement, illumination,
and encoder reconfiguration can all affect the signal. Treat it as a low-cost
hint for deciding when to run more authoritative processing.

Activity configurations are not guaranteed to produce equivalent events across
codecs or camera encode profiles. Each profile may require separate
calibration.

Only parsed, access-unit-aligned H.264 or H.265 Annex B input is supported.
NAL-aligned input and AVC, HVC1, or HEV1 length-prefixed input are outside the
caps contract. H.265 support is limited to base-layer streams and standard VCL
NAL types.

A dormant replay prefix starts only at an H.264 or H.265 IDR picture. H.265 CRA
and BLA pictures are classified as keyframes for activity analysis but are not
used as decoder-reset boundaries. A stream that provides only CRA random-access
points cannot establish a dormant replay prefix; use an encoder/parser
configuration that supplies periodic IDRs and repeats parameter sets with
`config-interval=-1`.

## Performance benchmarking

Run the deterministic hot-path benchmarks from the repository root:

```sh
cargo bench -p gst-plugin-nalsense --bench hot_path -- --noplot
```

The benchmark generates H.264 and H.265 Annex-B access units in memory and does
not require a camera, cached recording, or encoder plugin. Performance results
are meaningful as before/after comparisons on the same host; do not compare
absolute timings across different machines or environments.

## Development

From the repository root:

```sh
mise run pre-commit
mise run build
mise run package:check
```

The tests register the plugin statically and cover caps, properties,
pass-through metadata, parsing failures, activity transitions, replay ordering,
buffer bounds, wake/sleep behavior, stream invalidation, and run-scoped
counters.
