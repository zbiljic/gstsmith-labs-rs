//! Show one source on every screen of a video wall layout.
//!
//! Builds `videotestsrc ! tee` with one `queue ! wallwarp` branch per screen
//! of a layout file. By default it renders one frame and writes
//! `screen-<id>.png` for every screen, plus `source.png`. With `--html` it
//! also writes `screen-<id>.html`, a page showing that PNG stretched over the
//! whole display, and an `index.html` linking them. With `--print-launch` it
//! prints the equivalent live `gst-launch-1.0` command, with one window per
//! screen, instead.
//!
//! ```sh
//! cargo run -p gst-plugin-wallwarp --example wall-demo -- \
//!     [LAYOUT.json] [--output-dir DIR] [--source-size WIDTHxHEIGHT] \
//!     [--pattern NAME] [--html] [--print-launch]
//! ```

use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gst::prelude::*;
use wall_geometry::Layout;

const USAGE: &str = "usage: wall-demo [LAYOUT.json] [--output-dir DIR] \
                     [--source-size WIDTHxHEIGHT] [--pattern NAME] [--html] [--print-launch]";

struct Options {
    layout: PathBuf,
    output_dir: PathBuf,
    source_size: Option<(u32, u32)>,
    pattern: String,
    html: bool,
    print_launch: bool,
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        layout: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/three-screens.json").into(),
        output_dir: "wall-demo".into(),
        source_size: None,
        pattern: "smpte".to_owned(),
        html: false,
        print_launch: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--output-dir" => options.output_dir = value()?.into(),
            "--source-size" => {
                let size = value()?;
                options.source_size = Some(
                    parse_size(&size)
                        .ok_or_else(|| format!("--source-size {size:?} is not WIDTHxHEIGHT"))?,
                );
            }
            "--pattern" => options.pattern = value()?,
            "--html" => options.html = true,
            "--print-launch" => options.print_launch = true,
            "-h" | "--help" => return Err(USAGE.to_owned()),
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}\n{USAGE}")),
            _ => options.layout = arg.into(),
        }
    }
    Ok(options)
}

/// One step of a `gst-launch-1.0` description.
enum Step {
    /// The first element, or one linked from the previous step with `!`.
    Element {
        linked: bool,
        factory: String,
        properties: Vec<String>,
    },
    /// Start a branch from a named element's next request pad.
    Branch(String),
}

fn element(linked: bool, factory: &str, properties: &[String]) -> Step {
    Step::Element {
        linked,
        factory: factory.to_owned(),
        properties: properties.to_vec(),
    }
}

/// The source, a tee, and one warp branch per screen ending in `sink`.
fn pipeline(
    layout: &Layout,
    source: (u32, u32),
    options: &Options,
    sink: impl Fn(&str) -> Vec<Step>,
) -> Vec<Step> {
    let mut source_properties = vec![format!("pattern={}", options.pattern)];
    source_properties.push(if options.print_launch {
        "is-live=true".to_owned()
    } else {
        "num-buffers=1".to_owned()
    });
    let mut steps = vec![
        element(false, "videotestsrc", &source_properties),
        element(
            true,
            &format!(
                "video/x-raw,format=RGBA,width={},height={}",
                source.0, source.1
            ),
            &[],
        ),
        element(true, "tee", &["name=wall".to_owned()]),
    ];
    for screen in &layout.screens {
        steps.push(Step::Branch("wall".to_owned()));
        steps.push(element(true, "queue", &[]));
        steps.push(element(
            true,
            "wallwarp",
            &[
                format!("name={}", screen.id),
                format!("quad=\"{}\"", layout.source_quad(screen)),
                format!("output-width={}", screen.output.width),
                format!("output-height={}", screen.output.height),
            ],
        ));
        steps.extend(sink(&screen.id));
    }
    steps
}

/// Render steps in the repository's one-element-per-line style.
fn launch_command(steps: &[Step]) -> String {
    // Point at the plugin built with this example's profile.
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut lines = vec![format!(
        "GST_PLUGIN_PATH=\"$PWD/target/{profile}\" gst-launch-1.0"
    )];
    for step in steps {
        match step {
            Step::Element {
                linked,
                factory,
                properties,
            } => {
                let link = if *linked { "! " } else { "" };
                lines.push(format!("  {link}{factory}"));
                lines.extend(
                    properties
                        .iter()
                        .map(|property| format!("      {property}")),
                );
            }
            Step::Branch(name) => lines.push(format!("  {name}.")),
        }
    }
    lines.join(" \\\n")
}

fn launch_description(steps: &[Step]) -> String {
    let mut parts = Vec::new();
    for step in steps {
        match step {
            Step::Element {
                linked,
                factory,
                properties,
            } => {
                if *linked {
                    parts.push("!".to_owned());
                }
                parts.push(factory.clone());
                parts.extend(properties.iter().cloned());
            }
            Step::Branch(name) => parts.push(format!("{name}.")),
        }
    }
    parts.join(" ")
}

fn png_sink(name: &str) -> Vec<Step> {
    vec![
        element(true, "pngenc", &[]),
        element(true, "filesink", &[format!("name={name}")]),
    ]
}

/// A page showing `screen-<id>.png` stretched over the whole display, like the
/// calibration patterns, so the image lands where the layout measured.
fn viewer_page(id: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>wall-demo {id}</title>
<style>
html, body {{ margin: 0; height: 100%; overflow: hidden; background: #000; cursor: none; }}
img {{ display: block; width: 100vw; height: 100vh; }}
</style>
</head>
<body>
<img src="screen-{id}.png" alt="{id}">
<script>
// Cover the whole display, as the calibration pattern did: click or press a key.
for (const type of ["click", "keydown"]) {{
  addEventListener(type, () => document.documentElement.requestFullscreen?.());
}}
</script>
</body>
</html>
"#
    )
}

fn write_viewer_pages(layout: &Layout, directory: &Path) -> Result<(), Box<dyn Error>> {
    let mut links = String::new();
    for screen in &layout.screens {
        let id = &screen.id;
        std::fs::write(directory.join(format!("screen-{id}.html")), viewer_page(id))?;
        writeln!(
            links,
            "<li><a href=\"screen-{id}.html\">{id}</a> — {}x{}</li>",
            screen.output.width, screen.output.height
        )?;
    }
    let index = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>wall-demo</title>\n</head>\n<body>\n<p>Open each display's page on that \
         display and make it fullscreen.</p>\n<ul>\n{links}</ul>\n</body>\n</html>\n"
    );
    std::fs::write(directory.join("index.html"), index)?;
    Ok(())
}

fn render(layout: &Layout, source: (u32, u32), options: &Options) -> Result<(), Box<dyn Error>> {
    gst::init()?;
    gstwallwarp::plugin_register_static()?;
    let mut steps = pipeline(layout, source, options, |id| {
        png_sink(&format!("file-{id}"))
    });
    steps.push(Step::Branch("wall".to_owned()));
    steps.push(element(true, "queue", &[]));
    steps.extend(png_sink("source-file"));

    let pipeline = gst::parse::launch(&launch_description(&steps))?
        .downcast::<gst::Pipeline>()
        .map_err(|_element| "the description is not a pipeline")?;
    std::fs::create_dir_all(&options.output_dir)?;
    let mut outputs = vec![("source-file".to_owned(), "source.png".to_owned())];
    outputs.extend(layout.screens.iter().map(|screen| {
        (
            format!("file-{}", screen.id),
            format!("screen-{}.png", screen.id),
        )
    }));
    for (sink, file) in &outputs {
        pipeline
            .by_name(sink)
            .ok_or("missing file sink")?
            .set_property("location", options.output_dir.join(file));
    }

    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().ok_or("pipeline without a bus")?;
    let result = bus
        .iter_timed(gst::ClockTime::NONE)
        .find_map(|message| match message.view() {
            gst::MessageView::Eos(_) => Some(Ok(())),
            gst::MessageView::Error(err) => Some(Err(format!(
                "{}: {} ({})",
                err.src().map(GstObjectExt::path_string).unwrap_or_default(),
                err.error(),
                err.debug().unwrap_or_default()
            ))),
            _ => None,
        })
        .unwrap_or(Ok(()));
    pipeline.set_state(gst::State::Null)?;
    result?;

    for screen in &layout.screens {
        println!(
            "{}: {}x{} -> {}",
            screen.id,
            screen.output.width,
            screen.output.height,
            options
                .output_dir
                .join(format!("screen-{}.png", screen.id))
                .display()
        );
    }
    if options.html {
        write_viewer_pages(layout, &options.output_dir)?;
        println!(
            "pages: open {} and show each screen's page fullscreen on that display",
            options.output_dir.join("index.html").display()
        );
    }
    println!(
        "source: {}x{} -> {}",
        source.0,
        source.1,
        options.output_dir.join("source.png").display()
    );
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = options()?;
    let layout = Layout::from_json(&std::fs::read_to_string(&options.layout)?)?;
    let source = options
        .source_size
        .unwrap_or((layout.canvas.width, layout.canvas.height));
    layout.check_source_size(source.0, source.1)?;

    if options.print_launch {
        let steps = pipeline(&layout, source, &options, |_id| {
            vec![
                element(true, "videoconvert", &[]),
                element(true, "autovideosink", &[]),
            ]
        });
        println!("{}", launch_command(&steps));
        return Ok(());
    }
    render(&layout, source, &options)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("wall-demo: {err}");
            ExitCode::FAILURE
        }
    }
}
