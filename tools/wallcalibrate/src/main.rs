//! Command-line interface; run `wallcalibrate help`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use wall_geometry::{Layout, Point, Quad, Size};
use wallcalibrate::calibration::{self, Calibration, SolveOptions, Status};
use wallcalibrate::canvas::{self, CanvasSize, LayoutOptions, Mode};
use wallcalibrate::detect::{self, Detections};
use wallcalibrate::manifest::{Manifest, Placement};
use wallcalibrate::synth::{self, SynthOptions};
use wallcalibrate::{pattern, preview};

const USAGE: &str = "\
Calibrate an irregular video wall from one photo of AprilTag markers.

Usage: wallcalibrate <command> [arguments]

Commands:
  manifest  Assign markers to displays
              (--screen ID:WIDTHxHEIGHT ... | --layout LAYOUT.json) -o MANIFEST.json
              [--first-tag N] [--marker-size FRACTION] [--inset FRACTION]
  pattern   Write a fullscreen pattern page for every display
              MANIFEST.json -o DIRECTORY [--png]
  detect    Find markers in a photo with the built-in detector
              PHOTO -o DETECTIONS.json
  solve     Fit every display's corners in the photo from its markers
              MANIFEST.json DETECTIONS.json -o CALIBRATION.json
              [--preview PREVIEW.html] [--photo PHOTO] [--min-markers N]
  layout    Turn photo corners into a wallwarp canvas layout
              CALIBRATION.json -o LAYOUT.json [--mode viewpoint|planar]
              [--reference ID] [--reference-size WIDTHxHEIGHT]
              [--canvas WIDTH|WIDTHxHEIGHT] [--level ID] [--straighten]
              [--allow-incomplete] [--preview LAYOUT.svg]
  synth     Render a synthetic photo of a layout's displays, for testing
              MANIFEST.json LAYOUT.json -o PHOTO.png [--truth CALIBRATION.json]
              [--size WIDTHxHEIGHT] [--view X,Y,X,Y,X,Y,X,Y] [--black LEVEL]
              [--blur PASSES] [--noise AMPLITUDE] [--hide-tag TAG]...

`solve` exits with status 2 when a display has too few markers. Place it in the
preview or edit its corners in CALIBRATION.json, then run `layout`.";

/// Exit status for an incomplete calibration.
const INCOMPLETE: u8 = 2;

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    values: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    /// Split arguments into positionals, `--name VALUE` options, and `--name`
    /// flags. `-o` is short for `--output`.
    fn parse(args: impl Iterator<Item = String>, options: &[&str], flags: &[&str]) -> Result<Self> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let name = if arg == "-o" {
                Some("output")
            } else {
                arg.strip_prefix("--")
            };
            match name {
                Some(name) if flags.contains(&name) => parsed.flags.push(name.to_owned()),
                Some(name) if options.contains(&name) => {
                    let value = args
                        .next()
                        .with_context(|| format!("{arg} needs a value"))?;
                    parsed.values.push((name.to_owned(), value));
                }
                Some(_) => bail!("unknown option {arg}"),
                None => parsed.positional.push(arg),
            }
        }
        Ok(parsed)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.values
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    fn required(&self, name: &str) -> Result<&str> {
        self.value(name)
            .with_context(|| format!("--{name} is required"))
    }

    fn parsed<T: FromStr>(&self, name: &str, default: T) -> Result<T> {
        self.value(name).map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_err| anyhow!("--{name} {value:?} is not valid"))
        })
    }

    fn positional<const N: usize>(&self, names: [&str; N]) -> Result<[&str; N]> {
        let values: Vec<_> = self.positional.iter().map(String::as_str).collect();
        values.try_into().map_err(|values: Vec<&str>| {
            anyhow!(
                "expected {} but got {} argument(s)",
                if N == 0 {
                    "no arguments".to_owned()
                } else {
                    names.join(" ")
                },
                values.len()
            )
        })
    }
}

fn read(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
}

fn write(path: &str, contents: &str) -> Result<()> {
    let path = Path::new(path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn parse_size<T: FromStr>(text: &str) -> Result<(T, T)> {
    let parse = || {
        let (width, height) = text.split_once('x')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    };
    parse().with_context(|| format!("{text:?} is not WIDTHxHEIGHT"))
}

fn tags(manifest: &Manifest, id: &str) -> String {
    manifest.screen(id).map_or_else(String::new, |screen| {
        let tags: Vec<_> = screen
            .markers
            .iter()
            .map(|marker| marker.tag.to_string())
            .collect();
        tags.join(", ")
    })
}

fn manifest(args: &Args) -> Result<ExitCode> {
    args.positional([])?;
    let defaults = Placement::default();
    let placement = Placement {
        size: args.parsed("marker-size", defaults.size)?,
        inset: args.parsed("inset", defaults.inset)?,
    };
    let first_tag = args.parsed("first-tag", 0)?;
    let manifest = if let Some(layout) = args.value("layout") {
        ensure!(
            args.values("screen").next().is_none(),
            "use either --layout or --screen"
        );
        Manifest::from_layout(&Layout::from_json(&read(layout)?)?, first_tag, placement)?
    } else {
        let screens = args
            .values("screen")
            .map(|screen| {
                let (id, size) = screen
                    .rsplit_once(':')
                    .with_context(|| format!("--screen {screen:?} is not ID:WIDTHxHEIGHT"))?;
                let (width, height) = parse_size(size)?;
                Ok((id.to_owned(), Size { width, height }))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !screens.is_empty(),
            "give --screen ID:WIDTHxHEIGHT for each display, or --layout"
        );
        Manifest::generate(&screens, first_tag, placement)?
    };
    write(args.required("output")?, &manifest.to_json()?)?;
    for screen in &manifest.screens {
        println!(
            "{}: {}x{}, tags {}",
            screen.id,
            screen.output.width,
            screen.output.height,
            tags(&manifest, &screen.id)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn pattern(args: &Args) -> Result<ExitCode> {
    let [manifest_path] = args.positional(["MANIFEST.json"])?;
    let manifest = Manifest::from_json(&read(manifest_path)?)?;
    let directory = args.required("output")?;
    let written = pattern::write(&manifest, Path::new(directory), args.flag("png"))?;
    println!(
        "wrote {} files; open {} and show each display's page fullscreen on that display",
        written.len(),
        Path::new(directory).join("index.html").display()
    );
    Ok(ExitCode::SUCCESS)
}

fn detect(args: &Args) -> Result<ExitCode> {
    let [photo] = args.positional(["PHOTO"])?;
    let detections = detect::detect_file(Path::new(photo))?;
    write(args.required("output")?, &detections.to_json()?)?;
    let tags: Vec<_> = detections
        .markers
        .iter()
        .map(|marker| marker.tag.to_string())
        .collect();
    println!(
        "found {} markers in {}x{} photo: tags {}",
        detections.markers.len(),
        detections.photo.width,
        detections.photo.height,
        if tags.is_empty() {
            "none".to_owned()
        } else {
            tags.join(", ")
        }
    );
    Ok(ExitCode::SUCCESS)
}

fn solve(args: &Args) -> Result<ExitCode> {
    let [manifest_path, detections_path] = args.positional(["MANIFEST.json", "DETECTIONS.json"])?;
    let manifest = Manifest::from_json(&read(manifest_path)?)?;
    let detections = Detections::from_json(&read(detections_path)?)?;
    let options = SolveOptions {
        min_markers: args.parsed("min-markers", SolveOptions::default().min_markers)?,
    };
    let mut calibration = calibration::solve(&manifest, &detections, options);
    if let Some(photo) = args.value("photo") {
        calibration.photo.path = Some(photo.to_owned());
    }
    write(args.required("output")?, &calibration.to_json()?)?;

    for screen in &calibration.screens {
        let used = screen
            .markers
            .iter()
            .filter(|marker| marker.state == calibration::MarkerState::Used)
            .count();
        let status = match screen.status {
            Status::Complete => "complete",
            Status::Partial => "partial",
            Status::Missing => "MISSING",
            Status::Manual => "manual",
        };
        let rms = screen
            .rms_error
            .map_or_else(String::new, |rms| format!(", rms {rms:.2} px"));
        println!(
            "{}: {status}, {used}/{} markers{rms}",
            screen.id,
            screen.markers.len()
        );
        for note in &screen.notes {
            println!("  {note}");
        }
    }
    if !calibration.unknown_markers.is_empty() {
        let tags: Vec<_> = calibration
            .unknown_markers
            .iter()
            .map(|marker| marker.tag.to_string())
            .collect();
        println!("ignored tags not in the manifest: {}", tags.join(", "));
    }

    if let Some(path) = args.value("preview") {
        let photo = calibration.photo.path.as_deref().map(PathBuf::from);
        let uri = match photo.map(|photo| detect::open_photo(&photo)) {
            Some(Ok(photo)) => Some(preview::photo_data_uri(&photo)?),
            Some(Err(err)) => {
                eprintln!("warning: {err:#}; the preview has no photo (use --photo)");
                None
            }
            None => None,
        };
        write(
            path,
            &preview::calibration_page(&calibration, uri.as_deref())?,
        )?;
        println!("preview: {path}");
    }

    let missing = calibration.missing();
    if missing.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "incomplete calibration: no corners for {}. Retake the photo, or place the displays \
         in the preview (or edit the calibration), then run `wallcalibrate layout`.",
        missing.join(", ")
    );
    Ok(ExitCode::from(INCOMPLETE))
}

fn layout(args: &Args) -> Result<ExitCode> {
    let [calibration_path] = args.positional(["CALIBRATION.json"])?;
    let calibration = Calibration::from_json(&read(calibration_path)?)?;
    let mode = match args.value("mode").unwrap_or("viewpoint") {
        "viewpoint" => {
            ensure!(
                args.value("reference").is_none() && args.value("reference-size").is_none(),
                "--reference and --reference-size apply to --mode planar"
            );
            Mode::Viewpoint
        }
        "planar" => Mode::Planar {
            reference: args
                .value("reference")
                .context("--mode planar needs --reference ID, a display of known aspect ratio")?
                .to_owned(),
            size: args.value("reference-size").map(parse_size).transpose()?,
        },
        other => bail!("unknown mode {other:?}; use viewpoint or planar"),
    };
    let canvas = match args.value("canvas") {
        None => CanvasSize::Width(1920),
        Some(text) if text.contains('x') => {
            let (width, height) = parse_size(text)?;
            CanvasSize::Fit { width, height }
        }
        Some(text) => CanvasSize::Width(
            text.parse()
                .map_err(|_err| anyhow!("--canvas {text:?} is not WIDTH or WIDTHxHEIGHT"))?,
        ),
    };
    let options = LayoutOptions {
        mode,
        canvas,
        allow_incomplete: args.flag("allow-incomplete"),
        level: args.value("level").map(str::to_owned),
        straighten: args.flag("straighten"),
    };
    let generated = canvas::layout(&calibration, &options)?;
    write(args.required("output")?, &generated.layout.to_json()?)?;
    if let Some(path) = args.value("preview") {
        write(path, &preview::layout_svg(&generated.layout)?)?;
    }

    let size = generated.layout.canvas;
    match &options.mode {
        Mode::Viewpoint => println!(
            "{}x{} canvas from the camera's viewpoint: content looks right from where the \
             photo was taken",
            size.width, size.height
        ),
        Mode::Planar { reference, .. } => println!(
            "{}x{} canvas rectified onto the plane of {reference}: assumes every display is in \
             that plane",
            size.width, size.height
        ),
    }
    if let Some(units) = generated.units_per_pixel {
        println!("one canvas pixel is {units:.4} reference-size units");
    }
    if !generated.omitted.is_empty() {
        println!(
            "omitted displays without corners: {}",
            generated.omitted.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The canvas, fitted into 80% of the photo, seen slightly from the left.
fn default_view(photo: (u32, u32), canvas: Size) -> Quad {
    let (photo_width, photo_height) = (f64::from(photo.0), f64::from(photo.1));
    let (canvas_width, canvas_height) = (f64::from(canvas.width), f64::from(canvas.height));
    let scale = 0.8 * (photo_width / canvas_width).min(photo_height / canvas_height);
    let (width, height) = (canvas_width * scale, canvas_height * scale);
    let (x, y) = ((photo_width - width) / 2.0, (photo_height - height) / 2.0);
    Quad::new([
        Point::new(x + 0.04 * width, y + 0.06 * height),
        Point::new(x + width, y),
        Point::new(x + 0.97 * width, y + height),
        Point::new(x, y + 0.92 * height),
    ])
}

fn synth(args: &Args) -> Result<ExitCode> {
    let [manifest_path, layout_path] = args.positional(["MANIFEST.json", "LAYOUT.json"])?;
    let manifest = Manifest::from_json(&read(manifest_path)?)?;
    let layout = Layout::from_json(&read(layout_path)?)?;
    let defaults = SynthOptions::default();
    let (width, height) = args
        .value("size")
        .map(parse_size)
        .transpose()?
        .unwrap_or((defaults.width, defaults.height));
    let view = match args.value("view") {
        Some(text) => text.parse::<Quad>()?,
        None => default_view((width, height), layout.canvas),
    };
    let options = SynthOptions {
        width,
        height,
        black: args.parsed("black", defaults.black)?,
        blur: args.parsed("blur", defaults.blur)?,
        noise: args.parsed("noise", defaults.noise)?,
        hidden_tags: args
            .values("hide-tag")
            .map(|tag| {
                tag.parse()
                    .map_err(|_err| anyhow!("--hide-tag {tag:?} is not a tag"))
            })
            .collect::<Result<_>>()?,
        ..defaults
    };
    let placements = synth::view_of_layout(&layout, &view)?;
    let photo = synth::render(&manifest, &placements, &options)?;
    let output = args.required("output")?;
    photo
        .save(output)
        .with_context(|| format!("writing {output}"))?;
    if let Some(path) = args.value("truth") {
        let mut truth = synth::truth(&manifest, &placements, &options)?;
        truth.photo.path = Some(output.to_owned());
        write(path, &truth.to_json()?)?;
    }
    println!(
        "{width}x{height} synthetic photo of {} displays: {output}",
        placements.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn run(command: &str, rest: impl Iterator<Item = String>) -> Result<ExitCode> {
    let rest: Vec<_> = rest.collect();
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let rest = rest.into_iter();
    match command {
        "manifest" => manifest(&Args::parse(
            rest,
            &[
                "screen",
                "layout",
                "first-tag",
                "marker-size",
                "inset",
                "output",
            ],
            &[],
        )?),
        "pattern" => pattern(&Args::parse(rest, &["output"], &["png"])?),
        "detect" => detect(&Args::parse(rest, &["output"], &[])?),
        "solve" => solve(&Args::parse(
            rest,
            &["output", "preview", "photo", "min-markers"],
            &[],
        )?),
        "layout" => layout(&Args::parse(
            rest,
            &[
                "output",
                "mode",
                "reference",
                "reference-size",
                "canvas",
                "level",
                "preview",
            ],
            &["allow-incomplete", "straighten"],
        )?),
        "synth" => synth(&Args::parse(
            rest,
            &[
                "output", "truth", "size", "view", "black", "blur", "noise", "hide-tag",
            ],
            &[],
        )?),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    match run(&command, args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("wallcalibrate: {err:#}");
            ExitCode::FAILURE
        }
    }
}
