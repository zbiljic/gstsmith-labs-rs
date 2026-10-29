# wall-geometry

Layout configuration and perspective mapping for irregular video walls,
independent of GStreamer and of how streams reach the displays. It parses and
validates versioned layout files, computes checked homographies, and converts
display-local coordinates to canvas and source coordinates. Its only
dependencies are `serde` and `serde_json`.

## Coordinates

A source video is shown on a virtual canvas measured in pixels, with the origin
at the top-left and Y increasing downwards. Each display shows the canvas
region inside one quadrilateral; canvas areas outside every display are never
shown.

A display's corners are listed top-left, top-right, bottom-right, bottom-left
in the display's *own* orientation. They are the canvas positions of the
display's first pixel, the end of its first row, its last pixel, and the start
of its last row. A display mounted rotated 90° clockwise therefore lists the
canvas point near its physical top-right first. In this order an unmirrored
display winds clockwise on screen.

Display-local coordinates `(u, v)` span `[0, 1]²` across the display output.
Source coordinates span `[0, 1]²` across the source frame. Canvas positions
convert to source positions by dividing by the canvas size, so the source must
have the canvas aspect ratio. `Layout::check_source_size` accepts a source
whose width and height each differ from that ratio by less than one pixel.

## Layout format

```json
{
  "version": 1,
  "canvas": { "width": 1920, "height": 1080 },
  "screens": [
    {
      "id": "display-a",
      "output": { "width": 1280, "height": 720 },
      "corners": [[80, 100], [780, 90], [800, 500], [90, 520]]
    },
    {
      "id": "display-b",
      "output": { "width": 1920, "height": 1080 },
      "corners": [[1370, 60], [1330, 920], [1060, 900], [1100, 30]]
    }
  ]
}
```

`Layout::from_json` rejects:

- versions other than `1`, reported before any schema error;
- unknown fields, missing fields, and non-numeric or out-of-range values;
- widths and heights of zero or above 2147483647, the GStreamer caps limit;
- layouts without screens;
- screen IDs that are empty, longer than 64 bytes, contain characters other
  than ASCII letters, digits, `-`, `_`, and `.`, or repeat;
- corners that are non-finite, repeated, collinear, concave, or
  self-intersecting, or that would make an ill-conditioned transform;
- corners that wind counter-clockwise, which would mirror the display.

Screen order carries no meaning: IDs identify screens. Screens may overlap,
leave gaps, or extend past the canvas.

## API

- `Layout::from_json`, `Layout::to_json`, and `Layout::validate`.
- `Layout::source_quad(screen)` gives a screen's corners in normalized source
  coordinates. Its `Display` form is the `quad` property of `wallwarp`.
- `Screen::local_to_canvas` and `Layout::local_to_source` give the homography
  from display-local coordinates.
- `Homography::unit_square_to_quad`, `from_correspondences`, `fit`,
  `inverse`, `then`, and `map`. `fit` is a least-squares fit through four or
  more correspondences, with Hartley normalization. Construction rejects
  non-finite, singular, and ill-conditioned matrices. The conditioning check
  bounds `|det H| / ‖H‖_F³`, which does not depend on the projective scale.
- `Quad::orientation` validates a quadrilateral and reports whether it is
  mirrored; `Quad` parses from and formats to eight comma-separated numbers.

## Development

```sh
cargo test -p wall-geometry
cargo clippy -p wall-geometry --all-targets -- -D warnings
```
