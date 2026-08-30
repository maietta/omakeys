mod active_monitor;
mod atspi_scan;
mod config;
mod grid;
mod hints;
mod input_region;
mod ipc;
mod overlay;
mod pointer;
mod screencap;
mod vision;

use std::rc::Rc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use gtk4::prelude::*;

#[derive(Parser)]
#[command(name = "omg-keys")]
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
    /// Tell a running daemon to toggle the settings/cheat-sheet menu. Bind this to a genuine
    /// long-press (3s+) of Super -- see the Hyprland press/release timer-script pair in
    /// bindings.lua, not Hyprland's own `long_press` flag (confirmed not to discriminate
    /// hold duration at all -- see HANDOFF.md).
    Menu,
    /// Tell a running daemon to shut down.
    Quit,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon => run_daemon(),
        Commands::Toggle => ipc::send_command(ipc::Command::ToggleGrid),
        Commands::Hints => ipc::send_command(ipc::Command::ToggleHints),
        Commands::Menu => ipc::send_command(ipc::Command::ToggleMenu),
        Commands::Quit => ipc::send_command(ipc::Command::Quit),
    }
}

fn run_daemon() -> Result<()> {
    let listener = ipc::bind()?;
    let (sender, receiver) = async_channel::unbounded();
    ipc::spawn_listener_thread(listener, sender);

    let app = gtk4::Application::builder()
        .application_id("dev.omg-keys.Overlay")
        .build();

    app.connect_activate(move |app| {
        let overlay = Rc::new(overlay::Overlay::new(app));
        let receiver = receiver.clone();
        let app_for_quit = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = receiver.recv().await {
                log::info!("omg-keysd: received command {cmd:?}");
                match cmd {
                    ipc::Command::ToggleGrid => overlay.toggle(),
                    ipc::Command::ToggleHints => overlay.toggle_hints(),
                    ipc::Command::ToggleMenu => overlay.toggle_menu(),
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

