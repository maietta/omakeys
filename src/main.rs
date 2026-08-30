mod active_monitor;
mod atspi_scan;
mod config;
mod fullscan_viz;
mod grid;
mod growth_viz;
mod hints;
mod input_region;
mod ipc;
mod overlay;
mod pointer;
mod screencap;
mod settings_tui;
mod vision;

use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gtk4::prelude::*;

/// Which synthetic image visualize-fullscan-demo renders against -- see its own doc comment.
#[derive(Clone, Copy, clap::ValueEnum)]
enum DemoScenario {
    Band,
    Uniform,
}

#[derive(Parser)]
#[command(name = "omakeys")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the background daemon that owns the grid overlay (start this once, e.g. via
    /// Hyprland `exec-once`).
    Daemon,
    /// Tell a running daemon to toggle the grid overlay. Bind this to Right Shift / Super.
    Toggle,
    /// Tell a running daemon to toggle the AT-SPI button/text-field/scrollable/terminal
    /// hint visualization.
    Hints,
    /// Open the settings/cheat-sheet menu as a terminal UI (via `omarchy-launch-terminal`,
    /// respecting the user's default terminal). Bind this to a genuine long-press (3s+) of
    /// Super -- see the Hyprland press/release timer-script pair in bindings.lua, not
    /// Hyprland's own `long_press` flag (confirmed not to discriminate hold duration at all --
    /// see HANDOFF.md). Standalone -- doesn't need the daemon running.
    Menu,
    /// Run the settings TUI directly in the current terminal, blocking until Esc/q. This is
    /// what `Menu` launches in a new terminal window; run it yourself if you're already in a
    /// terminal and don't want a new window.
    SettingsTui,
    /// Tell a running daemon to shut down.
    Quit,
    /// Dev/debug tool: render an animated GIF of the experimental seed-growth vision
    /// detector (see vision.rs's "Seed-growth box detection" section) running against a real
    /// screenshot of the focused monitor -- seeds + rays, raw traced segments, merged,
    /// length-matched survivors, and the final detected box(es). Standalone (doesn't need the
    /// daemon running); crops to a fixed-size region so the seed/frame count stays watchable.
    VisualizeGrowth {
        /// Where to write the GIF (default: growth-viz.gif in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Crop width in pixels.
        #[arg(long, default_value_t = 640)]
        width: u32,
        /// Crop height in pixels.
        #[arg(long, default_value_t = 480)]
        height: u32,
        /// Crop's left edge, in the focused monitor's own pixel space. Centered if omitted.
        #[arg(long)]
        x: Option<u32>,
        /// Crop's top edge, in the focused monitor's own pixel space. Centered if omitted.
        #[arg(long)]
        y: Option<u32>,
    },
    /// Dev/debug tool: render an animated GIF of the full-scan line detector (see vision.rs's
    /// "Full-scan line detection" section) -- every row scanned left to right and every
    /// column top to bottom, tallying where a color change happens, keeping only positions a
    /// lot of them agree on. Standalone; same crop options as visualize-growth.
    VisualizeFullscan {
        /// Where to write the GIF (default: fullscan-viz.gif in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Crop width in pixels.
        #[arg(long, default_value_t = 640)]
        width: u32,
        /// Crop height in pixels.
        #[arg(long, default_value_t = 480)]
        height: u32,
        /// Crop's left edge, in the focused monitor's own pixel space. Centered if omitted.
        #[arg(long)]
        x: Option<u32>,
        /// Crop's top edge, in the focused monitor's own pixel space. Centered if omitted.
        #[arg(long)]
        y: Option<u32>,
        /// Extra filter beyond the plain density threshold: "peak" also requires a genuine
        /// local spike against the neighborhood (rejects a broad band of similarly-dense
        /// neighbors -- vision.rs's `is_local_peak`/`DIVIDER_PEAK_RATIO`); "uniformity" also
        /// requires a consistent edge magnitude along the line's own span (rejects a line
        /// whose step size varies a lot, e.g. text-glyph edges -- `edge_magnitude_std_dev`).
        /// Rejected-by-this-check bars turn orange instead of yellow in the final frame, so
        /// the two runs are comparable.
        #[arg(long, value_enum)]
        check: Option<fullscan_viz::CheckKind>,
    },
    /// Dev/debug tool: same as visualize-fullscan, but against a small synthetic image
    /// instead of a live screenshot -- shows a check's intended effect in isolation,
    /// decoupled from whatever a live capture happens to contain. --demo picks the scenario
    /// (independent of --check, so you can render the "before" baseline too): "band" is a
    /// wide broad band of alternating columns plus one isolated real divider (matches
    /// vision.rs's `full_scan_lines_with_peak_check_rejects_a_broad_band_but_keeps_a_real_spike`
    /// test); "uniform" is one perfectly-uniform-step line plus one wildly-variable-step line
    /// at the same density (matches
    /// `full_scan_lines_with_uniformity_check_rejects_a_variable_magnitude_line_but_keeps_a_uniform_one`).
    VisualizeFullscanDemo {
        /// Where to write the GIF (default: fullscan-demo.gif in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, value_enum)]
        demo: Option<DemoScenario>,
        #[arg(long, value_enum)]
        check: Option<fullscan_viz::CheckKind>,
    },
    /// Dev/debug tool: warp the *real* cursor to (x, y) on the focused monitor via the same
    /// virtual-pointer protocol the daemon itself uses. Not needed for normal use -- exists
    /// to pin the real cursor over a specific window before scripting synthetic keyboard
    /// input (e.g. wtype) into the overlay: Hyprland's `input:follow_mouse` (left at its
    /// default -- changing it globally just to help scripted testing isn't the right
    /// trade-off) continuously refocuses keyboard input to whatever's under the real cursor,
    /// which can silently steal the overlay's keyboard grab away mid-script if the cursor is
    /// left sitting wherever it last happened to be (e.g. over the terminal driving the
    /// script) instead of over the window actually being driven.
    MoveCursor {
        /// X position in the focused monitor's own logical pixels.
        #[arg(long)]
        x: f64,
        /// Y position in the focused monitor's own logical pixels.
        #[arg(long)]
        y: f64,
    },
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon => run_daemon(),
        Commands::Toggle => ipc::send_command(ipc::Command::ToggleGrid),
        Commands::Hints => ipc::send_command(ipc::Command::ToggleHints),
        Commands::Menu => run_menu(),
        Commands::SettingsTui => settings_tui::run(),
        Commands::Quit => ipc::send_command(ipc::Command::Quit),
        Commands::VisualizeGrowth { out, width, height, x, y } => {
            let (capture, window) = capture_crop(width, height, x, y)?;
            let out_path = out.unwrap_or_else(|| PathBuf::from("growth-viz.gif"));
            growth_viz::render(&capture.gray, capture.width, capture.height, window, &out_path)?;
            println!("wrote {}", out_path.display());
            Ok(())
        }
        Commands::VisualizeFullscan { out, width, height, x, y, check } => {
            let (capture, window) = capture_crop(width, height, x, y)?;
            let out_path = out.unwrap_or_else(|| PathBuf::from("fullscan-viz.gif"));
            let check = check.unwrap_or(fullscan_viz::CheckKind::None);
            fullscan_viz::render(&capture.gray, capture.width, capture.height, window, check, &out_path)?;
            println!("wrote {}", out_path.display());
            Ok(())
        }
        Commands::VisualizeFullscanDemo { out, demo, check } => {
            let (gray, w, h) = match demo.unwrap_or(DemoScenario::Band) {
                DemoScenario::Band => synthetic_band_and_spike(),
                DemoScenario::Uniform => synthetic_uniform_and_variable(),
            };
            let check = check.unwrap_or(fullscan_viz::CheckKind::None);
            let out_path = out.unwrap_or_else(|| PathBuf::from("fullscan-demo.gif"));
            fullscan_viz::render(&gray, w, h, (0, 0, w as i32, h as i32), check, &out_path)?;
            println!("wrote {}", out_path.display());
            Ok(())
        }
        Commands::MoveCursor { x, y } => {
            let (output_name, screen_w, screen_h) = active_monitor::focused_output_geometry()?;
            let mut ptr = pointer::VirtualPointer::new(Some(&output_name))?;
            ptr.move_to(x, y, screen_w, screen_h)?;
            Ok(())
        }
    }
}

/// Open the settings TUI in a new terminal window, respecting the user's configured default
/// terminal (`omarchy-launch-terminal`, an Omarchy wrapper around `xdg-terminal-exec`) rather
/// than hardcoding one. Spawns and returns immediately -- the terminal (and the `settings-tui`
/// process running inside it) outlives this short-lived CLI invocation.
fn run_menu() -> Result<()> {
    let exe = std::env::current_exe().context("finding omakeys' own executable path")?;
    std::process::Command::new("omarchy-launch-terminal")
        .arg(exe)
        .arg("settings-tui")
        .spawn()
        .context("launching omarchy-launch-terminal (is Omarchy installed?)")?;
    Ok(())
}

/// A 400x300 grayscale buffer with a wide "broad band" of alternating columns (x=60..180 --
/// every column registers a vertical edge on every row, mimicking ordinary text's fairly
/// uniform column-to-column structure) plus one genuinely isolated solid vertical line at
/// x=300 -- the same image `vision.rs`'s
/// `full_scan_lines_with_peak_check_rejects_a_broad_band_but_keeps_a_real_spike` test uses.
fn synthetic_band_and_spike() -> (Vec<u8>, u32, u32) {
    let (w, h) = (400u32, 300u32);
    let mut gray = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 60..180 {
            if x % 2 == 0 {
                gray[(y * w + x) as usize] = 255;
            }
        }
        gray[(y * w + 300) as usize] = 255;
    }
    (gray, w, h)
}

/// A 300x300 grayscale buffer with a perfectly-uniform-step vertical line at x=100 (a "real
/// divider": background 0, line always 200) and a same-density but wildly-variable-step line
/// at x=200 (a "text-like" line: alternates 210/10 every row) -- the same image `vision.rs`'s
/// `full_scan_lines_with_uniformity_check_rejects_a_variable_magnitude_line_but_keeps_a_uniform_one`
/// test uses.
fn synthetic_uniform_and_variable() -> (Vec<u8>, u32, u32) {
    let (w, h) = (300u32, 300u32);
    let mut gray = vec![0u8; (w * h) as usize];
    for y in 0..h {
        gray[(y * w + 100) as usize] = 200;
        gray[(y * w + 200) as usize] = if y % 2 == 0 { 210 } else { 10 };
    }
    (gray, w, h)
}

/// Capture the focused monitor and resolve a crop window within it, shared by the
/// visualize-* dev tools -- `x`/`y` center the crop when omitted.
fn capture_crop(
    width: u32,
    height: u32,
    x: Option<u32>,
    y: Option<u32>,
) -> Result<(screencap::Capture, (i32, i32, i32, i32))> {
    let output_name = active_monitor::focused_output_name()?;
    let capture = screencap::capture_output_gray(&output_name)?;
    let (cw, ch) = (width.min(capture.width), height.min(capture.height));
    let wx = x.unwrap_or((capture.width - cw) / 2).min(capture.width - cw) as i32;
    let wy = y.unwrap_or((capture.height - ch) / 2).min(capture.height - ch) as i32;
    Ok((capture, (wx, wy, cw as i32, ch as i32)))
}

fn run_daemon() -> Result<()> {
    let listener = ipc::bind()?;
    let (sender, receiver) = async_channel::unbounded();
    ipc::spawn_listener_thread(listener, sender);

    let app = gtk4::Application::builder()
        .application_id("dev.omakeys.Overlay")
        .build();

    app.connect_activate(move |app| {
        let overlay = Rc::new(overlay::Overlay::new(app));
        let receiver = receiver.clone();
        let app_for_quit = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = receiver.recv().await {
                log::info!("omakeysd: received command {cmd:?}");
                match cmd {
                    ipc::Command::ToggleGrid => overlay.toggle(),
                    ipc::Command::ToggleHints => overlay.toggle_hints(),
                    ipc::Command::Quit => {
                        app_for_quit.quit();
                        break;
                    }
                }
            }
        });
    });

    // No positional args are meaningful for a background daemon; avoid gtk parsing argv.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

