# Segmentation test fixtures

These small synthetic ONNX graphs contain constants, no trained weights or
external assets. They use the repository's Apache-2.0 license. Shared fixtures
cover encoder/decoder layouts, automatic segmentation layouts and nonfinite
output rejection across every public profile.

Keep the generated files checked in: ordinary `cargo test` reads them directly
without Python or network access, matching the other gstsmith inference tests.
To rebuild them from the repository root, with `uv` installed:

```sh
mise run //analytics/usls:fixtures:segment
```

The [generator](generate.py) validates each graph with the ONNX checker before
saving it. Tool versions live in [mise.toml](../../../mise.toml); the graphs use
ONNX opset 18 and IR 9. These developer tools are not plugin runtime dependencies.
After regeneration, run `mise run //analytics/usls:test` and include any changed
ONNX files alongside the generator change.
