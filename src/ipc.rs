//! Unix-socket IPC between the short-lived `omakeys toggle` CLI (invoked by Hyprland's
//! `bindr` on Right Shift / Super) and the long-running `omakeysd` daemon that owns the
//! overlay. The daemon's listener runs on a plain blocking thread and hands decoded
//! commands to the GTK/glib main loop over an `async-channel`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Command {
    /// Right Shift / Super was pressed: open the grid if hidden, clear it if shown.
    ToggleGrid,
    /// Scan AT-SPI for buttons/text-fields/scrollables/terminals and visualize them.
    ToggleHints,
    /// Super was held past the long-press threshold: open the settings/cheat-sheet menu.
    ToggleMenu,
    /// Cleanly stop the daemon.
    Quit,
}

pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("omakeys.sock")
}

/// Send a single command to a running daemon. Returns an error if no daemon is listening.
pub fn send_command(cmd: Command) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to omakeysd socket at {}", path.display()))?;
    let payload = serde_json::to_vec(&cmd)?;
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(())
}

/// Bind the daemon's socket, removing any stale file left behind by a previous run.
pub fn bind() -> Result<UnixListener> {
    let path = socket_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding omakeysd socket at {}", path.display()))?;
    Ok(listener)
}

/// Spawn a blocking thread that accepts connections on `listener` forever, decoding each
/// as a `Command` and forwarding it to `sender`. Intended to be paired with a receiver that
/// is drained on the glib main loop via `glib::spawn_future_local`.
pub fn spawn_listener_thread(listener: UnixListener, sender: async_channel::Sender<Command>) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut buf = Vec::new();
            if stream.read_to_end(&mut buf).is_err() {
                continue;
            }
            let Ok(cmd) = serde_json::from_slice::<Command>(&buf) else {
                log::warn!("omakeysd: ignoring malformed IPC message");
                continue;
            };
            if sender.send_blocking(cmd).is_err() {
                break; // receiver (main loop) is gone; shut the listener thread down.
            }
        }
    });
}

