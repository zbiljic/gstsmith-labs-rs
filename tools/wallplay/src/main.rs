//! Command-line interface; run `wallplay --help`.

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wallplay::app::{App, Config};
use wallplay::player::{Fill, PlayerOptions, Signalling, Sink, Warp};
use wallplay::source::{DEFAULT_SOURCE, DEFAULT_STANDBY, Prepared, Resolver, Source};

const USAGE: &str = "\
Calibrate a video wall from a photo and play one video across its displays in
web browsers: a test pattern, a file, a live stream or camera, or the video of
a web page such as YouTube.

Usage: wallplay [options]

Options:
  --listen ADDRESS:PORT      Serve the control and display pages here
                             [default: 0.0.0.0:8000]
  --state DIRECTORY          Keep the manifest, photo, calibration and layout
                             here [default: wallplay-state]
  --source PATTERN|URL|camera:ID|screen:N|PIPELINE
                             What to play first: a test pattern (bars, sweep,
                             ball, spokes, checkerboard, timecode, slate,
                             black); a URL such as file:///path/video.mp4,
                             rtsp://camera/stream, an HLS playlist, a WHEP
                             endpoint ending in /whep, or a web page such as
                             YouTube; a camera or screen of this machine, as
                             --list-devices shows them; or a gst-launch-1.0
                             description with one video output and at most
                             one audio output [default: bars]
  --standby PATTERN          Shown while the source connects, after it ends
                             and when it fails [default: slate]
  --play-once                Leave the standby showing when the source ends,
                             instead of looping clips and reconnecting streams
  --sound DISPLAY            Play the sound on this display; the others get it
                             muted. Without it, the streams carry no sound
  --yt-dlp PROGRAM           Finds the video of web pages [default: yt-dlp]
  --fps N                    Canvas frame rate [default: 30]
  --max-output PIXELS        Longest side of each display's stream [default: 1920]
  --warp auto|cpu|gpu        Where displays' frames are warped: gpu uses
                             OpenGL and leaves the CPU little but encoding;
                             auto picks gpu when OpenGL works [default: auto]
  --signalling-port PORT     First port of the per-display signalling servers;
                             displays take the first free ports from here
                             [default: 8443]
  --signalling-uri URI       Use one external signalling server instead, such
                             as ws://HOST:8443 from gst-webrtc-signalling-server;
                             browsers connect to the same URI
  --list-devices             List this machine's cameras and screens, and exit
  -h, --help                 Show this help

Open the control page, then open http://HOST:PORT/display/ID on every display.";

struct Options {
    listen: SocketAddr,
    config: Config,
    /// `None` to pick the GPU when OpenGL works.
    warp: Option<Warp>,
}

fn value<T: std::str::FromStr>(name: &str, value: Option<String>) -> Result<T> {
    let value = value.with_context(|| format!("{name} needs a value"))?;
    value
        .parse()
        .map_err(|_err| anyhow::anyhow!("{name} {value:?} is not valid"))
}

/// What the command line asks for.
enum Command {
    Help,
    ListDevices,
    Run(Box<Options>),
}

fn options(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut listen: SocketAddr = "0.0.0.0:8000".parse()?;
    let mut state_dir = "wallplay-state".into();
    let mut source = DEFAULT_SOURCE.to_owned();
    let mut standby = DEFAULT_STANDBY.to_owned();
    let mut restart = true;
    let mut sound = None;
    let mut resolver = Resolver::default();
    let mut fps = 30;
    let mut max_output = 1920;
    let mut first_port = 8443;
    let mut external = None;
    let mut warp = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = value(&arg, args.next())?,
            "--state" => state_dir = value::<String>(&arg, args.next())?.into(),
            "--source" => source = value(&arg, args.next())?,
            "--standby" => standby = value(&arg, args.next())?,
            "--play-once" => restart = false,
            "--sound" => sound = Some(value::<String>(&arg, args.next())?),
            "--yt-dlp" => resolver.program = value(&arg, args.next())?,
            "--fps" => fps = value(&arg, args.next())?,
            "--max-output" => max_output = value(&arg, args.next())?,
            "--signalling-port" => first_port = value(&arg, args.next())?,
            "--signalling-uri" => external = Some(value::<String>(&arg, args.next())?),
            "--warp" => {
                warp = match value::<String>(&arg, args.next())?.as_str() {
                    "auto" => None,
                    "cpu" => Some(Warp::Cpu),
                    "gpu" => Some(Warp::Gpu),
                    other => bail!("--warp {other:?} is not auto, cpu or gpu"),
                }
            }
            "--list-devices" => return Ok(Command::ListDevices),
            "-h" | "--help" => return Ok(Command::Help),
            _ => bail!("unknown argument {arg}"),
        }
    }
    if fps == 0 || max_output < 16 {
        bail!("--fps must be positive and --max-output at least 16");
    }
    if wallplay::source::pattern(&standby).is_none() {
        bail!("--standby {standby:?} is not a pattern");
    }
    if let Some(sound) = &sound
        && !wall_geometry::is_valid_screen_id(sound)
    {
        bail!("--sound {sound:?} is not a display ID");
    }
    let signalling = match external {
        Some(uri) => Signalling::External(uri),
        None => Signalling::BuiltIn {
            host: listen.ip().to_string(),
            first_port,
        },
    };
    Ok(Command::Run(Box::new(Options {
        listen,
        config: Config {
            state_dir,
            player: PlayerOptions {
                source: Prepared::prepare(&Source::parse(&source), &resolver)
                    .context("--source")?,
                standby,
                restart,
                fps,
                sink: Sink::WebRtc(signalling),
                fill: Fill::default(),
                resolver,
                // Set from the sound display whenever a player starts.
                audio: false,
                // Set once GStreamer can tell whether OpenGL works.
                warp: Warp::Cpu,
            },
            poll_timeout: Duration::from_secs(25),
            max_output,
            sound,
        },
        warp,
    })))
}

/// This machine's address on the network other devices use, if any; finding
/// it sends no packets.
fn lan_address() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn run(mut options: Options) -> Result<()> {
    gst::init()?;
    wallplay::register_wallwarp()?;
    if options.warp != Some(Warp::Cpu) {
        println!("Checking whether OpenGL can warp...");
    }
    let (warp, fallback) = match options.warp {
        Some(Warp::Cpu) => (Warp::Cpu, None),
        Some(Warp::Gpu) => {
            wallplay::player::check_gpu_warp()
                .map_err(|reason| anyhow::anyhow!("--warp gpu: {reason}"))?;
            (Warp::Gpu, None)
        }
        None => match wallplay::player::check_gpu_warp() {
            Ok(()) => (Warp::Gpu, None),
            Err(reason) => (Warp::Cpu, Some(reason)),
        },
    };
    options.config.player.warp = warp;
    let listener = TcpListener::bind(options.listen)
        .with_context(|| format!("listening on {}", options.listen))?;
    let address = listener.local_addr()?;
    let host = if address.ip().is_unspecified() {
        lan_address().map_or_else(|| "localhost".to_owned(), |ip| ip.to_string())
    } else {
        address.ip().to_string()
    };
    let state_dir = options.config.state_dir.display().to_string();
    let app = App::new(options.config)?;
    println!("Control page:  http://{host}:{}/", address.port());
    println!(
        "Display pages: http://{host}:{}/display/<id>",
        address.port()
    );
    println!("State:         {state_dir}");
    match (warp, fallback) {
        (Warp::Gpu, _) => println!("Warping:       on the GPU, through OpenGL"),
        (Warp::Cpu, None) => println!("Warping:       on the CPU"),
        (Warp::Cpu, Some(reason)) => {
            println!("Warping:       on the CPU; OpenGL did not warp: {reason}");
        }
    }
    wallplay::http::serve(&listener, &app.handler())
}

/// Print the cameras and screens a source can name.
fn list_devices() -> Result<()> {
    gst::init()?;
    let devices = wallplay::capture::devices();
    if devices.is_empty() {
        println!("No cameras or screens found.");
    }
    for device in devices {
        let size = device
            .size
            .map(|size| format!(" ({size})"))
            .unwrap_or_default();
        let source = match device.kind {
            wallplay::capture::Kind::Camera => format!("camera:{}", device.id),
            wallplay::capture::Kind::Screen => format!("screen:{}", device.id),
        };
        println!("{source}\t{}{size}", device.name);
    }
    if !wallplay::capture::SCREEN_CAPTURE {
        println!("Screens can be captured on macOS only.");
    }
    Ok(())
}

fn main() -> ExitCode {
    let done = |result: Result<()>| match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wallplay: {error:#}");
            ExitCode::FAILURE
        }
    };
    match options(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::ListDevices) => done(list_devices()),
        Ok(Command::Run(options)) => done(run(*options)),
        Err(error) => {
            eprintln!("wallplay: {error:#}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
