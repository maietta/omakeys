//! Virtual mouse pointer control via the wlr-virtual-pointer-unstable-v1 Wayland protocol.
//! This is a separate low-level Wayland connection from the one GTK uses for the overlay.
//!
//! The virtual pointer is bound to a specific `wl_output` (matched by connector name, e.g.
//! "eDP-1" or "DP-3") whenever possible, so that `move_to` coordinates are simply the target
//! monitor's own logical pixels -- no need to reason about multi-monitor global offsets.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// Linux input-event button codes (see linux/input-event-codes.h).
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;

#[derive(Default)]
struct State {
    /// Connector name (e.g. "DP-3") -> bound wl_output, filled in as `Event::Name` arrives.
    outputs_by_name: HashMap<String, wl_output::WlOutput>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs_by_name.insert(name, proxy.clone());
        }
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// A handle for injecting absolute mouse motion and clicks, using the compositor's
/// virtual-pointer protocol (requires the compositor to support
/// `zwlr_virtual_pointer_manager_v1`, which wlroots/Hyprland does).
pub struct VirtualPointer {
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
}

impl VirtualPointer {
    /// Connect and create a virtual pointer. If `output_name` is given and matches a
    /// currently-connected output (e.g. "eDP-1"), the pointer is bound to just that output,
    /// so `move_to` coordinates are that output's own logical pixels. Otherwise the pointer
    /// spans the compositor's whole (multi-monitor) global layout.
    pub fn new(output_name: Option<&str>) -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let mut state = State::default();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=8, ())
            .map_err(|e| anyhow!("compositor has no wl_seat: {e}"))?;

        // Bind every advertised output so we receive their `name` events; version 4 is
        // required for wl_output::name.
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let _: wl_output::WlOutput =
                    globals.registry().bind(global.name, global.version.min(4), &qh, ());
            }
        }
        queue.roundtrip(&mut state)?;

        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
            anyhow!(
                "compositor does not support zwlr_virtual_pointer_manager_v1 \
                 (needed to move the mouse from omakeys): {e}"
            )
        })?;

        let target_output = output_name.and_then(|name| state.outputs_by_name.get(name));
        if output_name.is_some() && target_output.is_none() {
            log::warn!(
                "omakeys: requested output {output_name:?} not found, falling back to \
                 global (multi-monitor) pointer coordinates"
            );
        }

        let pointer = manager.create_virtual_pointer_with_output(Some(&seat), target_output, &qh, ());
        queue.roundtrip(&mut state)?;

        Ok(Self { queue, state, pointer })
    }

    fn now_ms() -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32
    }

    fn flush(&mut self) -> Result<()> {
        self.queue.flush()?;
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    /// Move the cursor to an absolute position within the bound output (or global layout if
    /// no specific output was bound). `screen_w`/`screen_h` must match the logical size of
    /// that same coordinate space, and are used directly as the protocol's x/y extents (as
    /// wlroots-based compositors expect real pixel extents, not a normalized range).
    pub fn move_to(&mut self, x: f64, y: f64, screen_w: f64, screen_h: f64) -> Result<()> {
        let t = Self::now_ms();
        self.pointer
            .motion_absolute(t, x as u32, y as u32, screen_w as u32, screen_h as u32);
        self.pointer.frame();
        self.flush()
    }

    /// Press and release a mouse button (e.g. `BTN_LEFT`) at the current cursor position.
    pub fn click(&mut self, button: u32) -> Result<()> {
        self.press(button)?;
        self.release(button)
    }

    /// Press a mouse button and hold it down, without releasing -- pair with `release()` for
    /// click-and-drag (e.g. text selection): press, then `move_to()` one or more times while
    /// held, then release.
    pub fn press(&mut self, button: u32) -> Result<()> {
        let t = Self::now_ms();
        self.pointer
            .button(t, button, wayland_client::protocol::wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        self.flush()
    }

    /// Release a button previously held down with `press()`.
    pub fn release(&mut self, button: u32) -> Result<()> {
        let t = Self::now_ms();
        self.pointer
            .button(t, button, wayland_client::protocol::wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.flush()
    }
}

