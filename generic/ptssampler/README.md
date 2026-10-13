# GStreamer PTS sampler plugin

The `ptssampler` plugin provides `ptssampler`, which passes the first buffer
of every fixed period of running time and drops the others. It accepts any
caps and never modifies the buffers it passes.

Unlike `videorate drop-only=true max-rate=N`, the period is any duration
(for example 150 ms, which is not a whole frame rate), the element works on
any media type, and caps (including `framerate`) are left unchanged.

## Behavior

A buffer at running time `t` belongs to period
`floor((t - phase) / period)`. The first buffer of a period that is later
than the last passed period is passed; every other buffer is dropped. Periods
restart on stream start, a new segment, and flush stop. Buffers without a PTS
cannot be placed in a period and pass through. `period=0` passes every
buffer.

## Properties

All properties are mutable only through READY.

| Property | Default | Meaning |
|---|---:|---|
| `period` | `1000000000` | Period length in nanoseconds of running time; `0` passes everything |
| `phase` | `0` | Running time in nanoseconds at which periods start |

## Example

Keep one frame every 150 ms. Run from the repository root after
`mise run build`:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  videotestsrc \
      num-buffers=30 \
  ! ptssampler \
      period=150000000 \
  ! fakesink \
      silent=false \
  -v
```

## Development

```sh
cargo check -p gst-plugin-ptssampler --all-targets
cargo test -p gst-plugin-ptssampler --all-targets
cargo clippy -p gst-plugin-ptssampler --all-targets -- -D warnings
```
