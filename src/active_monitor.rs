//! Ask Hyprland which monitor/window is currently focused, so the grid overlay opens there
//! and hint scans target the right app.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct HyprMonitor {
    id: i64,
    name: String,
    focused: bool,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    #[serde(rename = "activeWorkspace")]
    active_workspace: HyprWorkspaceRef,
}

#[derive(Deserialize)]
struct HyprWorkspaceRef {
    id: i64,
}

/// The connector name (e.g. "eDP-1", "DP-3") of the currently focused monitor, per `hyprctl`.
pub fn focused_output_name() -> Result<String> {
    let output = std::process::Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .context("running `hyprctl -j monitors`")?;
    let monitors: Vec<HyprMonitor> =
        serde_json::from_slice(&output.stdout).context("parsing `hyprctl -j monitors` output")?;
    monitors
        .into_iter()
        .find(|m| m.focused)
        .map(|m| m.name)
        .context("no focused monitor reported by hyprctl")
}

/// The focused monitor's connector name plus its logical size (`width`/`height` divided by
/// `scale`, matching the logical-pixel space `pointer::VirtualPointer::move_to` and the
/// overlay itself both operate in).
pub fn focused_output_geometry() -> Result<(String, f64, f64)> {
    let output = std::process::Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .context("running `hyprctl -j monitors`")?;
    let monitors: Vec<HyprMonitor> =
        serde_json::from_slice(&output.stdout).context("parsing `hyprctl -j monitors` output")?;
    monitors
        .into_iter()
        .find(|m| m.focused)
        .map(|m| (m.name, m.width / m.scale, m.height / m.scale))
        .context("no focused monitor reported by hyprctl")
}

#[derive(Deserialize)]
struct HyprClient {
    title: String,
    monitor: i64,
    workspace: HyprWorkspaceRef,
    mapped: bool,
    at: (f64, f64),
    size: (f64, f64),
}

/// A window visible on the focused monitor, with its position translated from Hyprland's
/// global coordinate space into coordinates local to that monitor (i.e. relative to the
/// monitor's own top-left corner). AT-SPI element coordinates are relative to their own
/// window's origin (see atspi_scan.rs), so this offset is what turns those into monitor-local
/// pixels: `window.x + element.x`, `window.y + element.y`. `w`/`h` let the vision pipeline
/// restrict itself to actual window bounds instead of the whole monitor (e.g. skipping empty
/// desktop/wallpaper) -- see `vision::filter_to_windows`.
#[derive(Clone)]
pub struct FocusedWindow {
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Every mapped window on the focused monitor's active workspace, per `hyprctl`. Used to
/// match the corresponding AT-SPI application frames (AT-SPI has no notion of "visible on
/// this monitor" independent of the toolkit's own window tracking) and to translate their
/// element coordinates into this monitor's local space.
pub fn focused_monitor_windows() -> Result<Vec<FocusedWindow>> {
    let monitors_out = std::process::Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .context("running `hyprctl -j monitors`")?;
    let monitors: Vec<HyprMonitor> = serde_json::from_slice(&monitors_out.stdout)
        .context("parsing `hyprctl -j monitors` output")?;
    let focused = monitors
        .into_iter()
        .find(|m| m.focused)
        .context("no focused monitor reported by hyprctl")?;

    let clients_out = std::process::Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .context("running `hyprctl -j clients`")?;
    let clients: Vec<HyprClient> = serde_json::from_slice(&clients_out.stdout)
        .context("parsing `hyprctl -j clients` output")?;

    Ok(clients
        .into_iter()
        .filter(|c| {
            c.mapped && c.monitor == focused.id && c.workspace.id == focused.active_workspace.id
        })
        .map(|c| FocusedWindow {
            title: c.title,
            x: c.at.0 - focused.x,
            y: c.at.1 - focused.y,
            w: c.size.0,
            h: c.size.1,
        })
        .collect())
}
