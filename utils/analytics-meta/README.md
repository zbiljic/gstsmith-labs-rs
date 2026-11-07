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

## Generic `x-*` documents

`schema` writes and reads model-agnostic documents for what GStreamer 1.28
has no Mtd type for. A consumer that reads only these and the standard Mtds
works with any producer that writes them.

| Schema | Attached to | Content |
| --- | --- | --- |
| `x-polygon` | an `ODMtd`, `CONTAIN` and `IS_PART_OF` | `{"rings": [[x, y, ...], ...]}`, normalized 0..1 |
| `x-relation` | its own node: subject `RELATE_TO` node `RELATE_TO` object | `{"predicate": "holding"}`; the direct subject `RELATE_TO` object edge is kept too |
| `x-caption` | the frame | `{"text": "..."}` |
| `x-text` | an `ODMtd` labelled `text-line`, `CONTAIN` and `IS_PART_OF` | `{"string": "...", "score": 0.7}` |
| `x-provenance` | the frame | `{"pts": ..., "running-time": ..., "producer": "..."}` |

No document stores Mtd ids: upstream's transforms renumber ids but copy
relation edges, so predicates live in their own nodes. Upstream's
`set_relation` assigns a cell, so the writers OR their relation into what is
already there. Per-object documents normally hang under an `ODMtd`, but any
Mtd may contain them; polygon coordinates up to 0.001 outside 0..1 are
clamped, anything further is rejected. Documents are copied
unchanged while `ODMtd` boxes are rescaled, which is why polygons use
normalized coordinates. Frame documents are found by schema. Writers check
ids first, so a failed call leaves no orphan document. Tests run real
`videoconvertscale` and `videoconvert` transforms and writable copies.
GStreamer 1.28.0 to 1.28.2 drop every `ODMtd`, and with it its edges, when
a buffer is copied, usually by an in-place element making a shared buffer
writable (fixed in 1.28.3); documents survive, but the documents under a
dropped box lose their link to it.

## Recording

`record::relation_meta` turns a relation meta into JSON: every Mtd with its
upstream type name and type-specific fields, its outgoing relations and, for
`JsonMtd`, its schema and parsed JSON (keys sorted, numbers normalized, no
value lost; `f32` fields at `f32` precision). Unknown Mtd types are recorded
by name only.

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
