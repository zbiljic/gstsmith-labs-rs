# NALSense

The `nalsense` plugin provides inexpensive activity hints and bounded replay
for parsed H.264 streams. It operates on compressed access units and does not
decode pixels.

The plugin contains two elements:

- `nalsenseactivity` passes H.264 access units through unchanged and reports
  encoded-size activity transitions.
- `nalsensereplay` retains a bounded prefix from the latest IDR while dormant,
  then replays that prefix before forwarding live data after a wake.

The plugin requires GStreamer 1.24 or newer.

## Input contract

Both elements use the same always-present sink and source caps:

```text
video/x-h264,
    parsed=(boolean)true,
    stream-format=(string)byte-stream,
    alignment=(string)au
```

Place `h264parse` before the plugin to assemble access units and convert the
stream to Annex B framing.

## Activity analysis

This pipeline prints element messages while preserving every compressed
buffer:

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

`nalsenseactivity` classifies I, P, and B pictures from slice headers. It sums
VCL payload bytes within each access unit, keeps independent exponentially
weighted baselines for P and B pictures, and excludes keyframes from activity
transitions. Consecutive anomalous frames start activity; consecutive normal
frames clear it.

| Property | Default | Purpose |
| --- | ---: | --- |
| `stream-id` | `stream` | Identifier included in event messages |
| `activity-threshold` | `3.0` | Minimum pre-update standard-deviation score |
| `baseline-alpha` | `0.02` | Baseline update weight in `(0, 1]` |
| `activity-min-frames` | `3` | Consecutive anomalies required to start |
| `activity-clear-frames` | `4` | Consecutive normal frames required to stop |
| `warmup-frames` | `30` | Observations learned before transitions are eligible |
| `post-idr-guard-frames` | `0` | Post-IDR frames that learn but cannot change state |

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
h264parse ! nalsenseactivity ! nalsensereplay ! decoder
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
Stream changes and discontinuities discard the retained prefix and reset H.264
scanner state. Active and pending-wake requests survive that invalidation; a
pending sleep becomes dormant because decoding continuity has already ended.
Counters remain cumulative until a new PAUSED run begins.

Each wake posts a `nalsense-replay` message containing `type=wake`, `reason`,
`replayed-frames`, and `replayed-bytes`.

## Optional diagnostics

The `qp-diagnostics` Cargo feature parses SPS/PPS and slice headers to include
the luma QP in TRACE logging. It does not change activity scoring or the public
event schema.

## Limitations

Encoded-size activity is a global stream heuristic, not spatial object
detection. Rate-control changes, scene cuts, camera movement, illumination,
and encoder reconfiguration can all affect the signal. It should be treated as
a low-cost hint for deciding when to run more authoritative processing.

Only parsed, access-unit-aligned H.264 Annex B input is supported. NAL-aligned
input, AVC length-prefixed input, and H.265 are outside the current caps
contract.

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
