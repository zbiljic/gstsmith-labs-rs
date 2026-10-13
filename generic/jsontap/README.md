# GStreamer JSON tap plugin

The `jsontap` plugin provides `jsontap`, a passthrough element that records
what crosses it as JSON lines. It is meant for inspecting and testing
pipelines stage by stage: place a tap between two elements, run the pipeline,
and read what went through.

## Records

`jsontap` writes one JSON object per line to `location`, truncating the file
when the element starts. Data, timestamps, flags, and metas pass downstream
unchanged; the element accepts any caps.

Serialization and file writes run synchronously on the streaming thread. A slow
output filesystem can stall the pipeline. Records are buffered and flushed at
EOS or when the element stops.

- `{"kind":"caps","caps":"..."}`: each negotiated caps change.
- `{"kind":"buffer",...}`: every buffer, with `seq` (zero-based count since
  start), `pts`, `dts`, and `duration` in nanoseconds or `null`, `offset` and
  `offset_end` or `null`, `flags` (for example `delta-unit`, `discont`),
  `size` in bytes, and `metas`.
- `{"kind":"eos"}`: end of stream; the file is flushed.

Each entry of `metas` names the meta API. Custom metas (`GstCustomMeta`)
are named by their registered name and include their structure `fields`:
numbers, booleans, and strings as JSON values, arrays and lists as JSON
arrays, nested structures as objects, anything else in its `GStreamer`
serialized form.

```json
{"kind":"buffer","seq":0,"pts":0,"dts":null,"duration":33333333,"offset":0,"offset_end":1,"flags":["discont"],"size":8294400,"metas":[{"api":"GstVideoMetaAPI"},{"api":"MyFrameMeta","fields":{"index":0}}]}
```

## Properties

| Property | Default | Meaning |
|---|---|---|
| `location` | unset (required) | JSON lines file to write; mutable only through READY |

## Example

Run from the repository root after `mise run build`:

```sh
GST_PLUGIN_PATH="$PWD/target/debug" gst-launch-1.0 \
  videotestsrc \
      num-buffers=3 \
  ! jsontap \
      location=frames.jsonl \
  ! fakesink
```

## Development

```sh
cargo check -p gst-plugin-jsontap --all-targets
cargo test -p gst-plugin-jsontap --all-targets
cargo clippy -p gst-plugin-jsontap --all-targets -- -D warnings
```
