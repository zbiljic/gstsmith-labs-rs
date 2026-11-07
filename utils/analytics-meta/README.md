# analytics-meta

Safe bridges to GStreamer 1.28 analytics relation metadata that the
gstreamer-analytics bindings do not cover.

## `JsonMtd`

A schema-tagged JSON document stored as an Mtd in
`GstAnalyticsRelationMeta`, under the upstream impl name `x-json`.

- **Layout.** `{u32 schema_len; u32 json_len; u8 bytes[]}` in native byte
  order, then the schema and the JSON text in UTF-8, without NUL
  terminators. The type has no transform or clear function, so upstream's
  relation-meta transform copies documents unchanged.
- **Limits.** Writers refuse documents over 1 MiB and schema names outside
  `[a-z0-9._/-]` or over 256 bytes. Readers ignore anything malformed or
  oversized instead of failing.
- **One type across plugins.** Upstream identifies an Mtd type by the address
  of its `GstAnalyticsMtdImpl`, and every plugin library linking this crate
  has its own copy of that static. The first library to use the type installs
  its static in a process-wide slot, qdata on the default `GstRegistry` keyed
  `gstsmith.analytics.x-json.v1`, with an atomic compare-and-swap. Every
  library then uses the winner, so a document written by one plugin is read
  by another. Correctness rests on `g_object_replace_qdata` being an atomic
  compare-and-exchange; the race test is a regression check, not a proof.
- **Versioning.** The winner's impl serves every library's documents, so
  `x-json` v1 has no transform or clear callbacks by contract. Changing the
  layout or adding a callback renames the impl to `x-json-2` and the slot key
  to `.v2`, together.

## Segmentation

`segmentation::add` attaches a full-frame binary GRAY8 mask as a
`SegmentationMtd` (region 0 background, region 1 foreground), and
`segmentation::mask` returns an owned reference to a mask buffer.
Dimensions, pixels and strides are validated before FFI; tests exercise deep
copies and buffer lifetimes. Replace this module with the upstream safe
binding when one exists.

## Tests

`fixtures/` holds two test-only plugin libraries. Their test loads both with
`gst::Plugin::load_file`, races the first use of the type from two threads in
fresh processes, and exchanges documents in both directions. It also seeds
the slot as a foreign library would, to check that a foreign `x-json` impl is
adopted and a wrongly named one ignored. The fixtures build into
`target/debug` as `jsonmtdfixturea` and `jsonmtdfixtureb`, so
`GST_PLUGIN_PATH="$PWD/target/debug"` lists them too; they are not installed
or packaged.

The workspace lints deny `unsafe_code`. This crate opts in only its FFI
modules and items, with `#[expect(unsafe_code, reason = "...")]`; plugin
crates contain no unsafe code.
