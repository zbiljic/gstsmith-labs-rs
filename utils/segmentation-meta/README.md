# segmentation-meta

A narrow safe bridge to GStreamer 1.28 segmentation analytics, pending safe
bindings in gstreamer-analytics (currently 0.25.4). It attaches owned binary
GRAY8 masks and retrieves owned mask references. Dimensions, pixels and strides
are validated before FFI; tests exercise deep copies and buffer lifetimes.

This crate alone permits documented unsafe FFI calls. Plugin crates continue
to inherit the workspace's `unsafe_code = forbid`. Replace the bridge with the
upstream safe binding when available. No independent native dependency is added.
