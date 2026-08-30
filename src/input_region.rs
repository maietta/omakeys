//! Makes our own GTK4 layer-shell overlay window pointer-transparent, so it never
//! intercepts clicks or motion meant for whatever's underneath it.
//!
//! omg-keys is 100% keyboard-driven -- we never need our own surface to receive real
//! pointer input -- but `gtk4-layer-shell` doesn't expose any input-region control of its
//! own, and a mapped GTK surface's default input region is the whole surface. Since our
//! overlay is full-screen and topmost (`Layer::Overlay`) for as long as it's open, that
//! means it's the thing every pointer event at the cursor's position actually goes to:
//! confirmed live, both (a) clicks sent through our own virtual pointer were getting
//! swallowed before ever reaching the app underneath, and (b) the *real* mouse couldn't
//! move the cursor at all while the overlay was open. Setting an empty input region fixes
//! both -- pointer hit-testing skips our surface entirely, while keyboard focus (a
//! completely separate Wayland mechanism, driven by `gtk4_layer_shell::KeyboardMode`) is
//! untouched.
//!
//! `gdk4-wayland` exposes the raw `wl_surface` and `wl_compositor` for GDK's own Wayland
//! connection (`WaylandSurface::wl_surface()` / `WaylandDisplay::wl_compositor()`), both as
//! ordinary `wayland_client` proxies -- the same crate `pointer.rs` already uses for its own,
//! *separate* connection. Reaching this API requires the `wayland_crate` gdk4-wayland
//! feature; the plain surface pointer accessible without it isn't usable from safe Rust.

use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4 as gtk;
use gtk4::prelude::*;
use wayland_client::protocol::wl_region;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

/// Set an empty input region on `window`'s surface. A no-op (logged, not fatal) if the
/// surface isn't realized yet or we're somehow not running on Wayland at all -- neither
/// should happen in practice, since this is only ever wired up via `connect_realize` on a
/// `gtk4-layer-shell` window, but losing pointer-passthrough is a much smaller problem than
/// the overlay refusing to run.
pub fn make_surface_click_through(window: &gtk::ApplicationWindow) {
    let Some(surface) = window.surface() else {
        log::warn!("omg-keys: window has no surface yet, can't set input region");
        return;
    };
    let Some(wayland_surface) = surface.downcast_ref::<gdk4_wayland::WaylandSurface>() else {
        log::warn!("omg-keys: not on a Wayland GDK backend, skipping input-region fix");
        return;
    };
    let Some(wl_surface) = wayland_surface.wl_surface() else {
        log::warn!("omg-keys: could not get the raw wl_surface, skipping input-region fix");
        return;
    };
    let Some(wayland_display) =
        gtk4::prelude::WidgetExt::display(window).downcast_ref::<gdk4_wayland::WaylandDisplay>().cloned()
    else {
        log::warn!("omg-keys: not on a Wayland GDK display, skipping input-region fix");
        return;
    };
    let Some(compositor) = wayland_display.wl_compositor() else {
        log::warn!("omg-keys: could not get wl_compositor, skipping input-region fix");
        return;
    };

    // `WaylandDisplay::connection()` isn't public, so reconstruct a `Connection` the
    // standard way: any proxy on the connection we want can hand back an (weak) backend
    // handle for it.
    let Some(backend) = compositor.backend().upgrade() else {
        log::warn!("omg-keys: wayland backend already gone, skipping input-region fix");
        return;
    };
    let connection = Connection::from_backend(backend);
    let mut event_queue = connection.new_event_queue::<State>();
    let qh = event_queue.handle();

    let region = compositor.create_region(&qh, ());
    // No add_rectangle() calls -- an empty region means "accepts no pointer input at all".
    wl_surface.set_input_region(Some(&region));
    wl_surface.commit();
    region.destroy();

    if let Err(e) = event_queue.roundtrip(&mut State) {
        log::warn!("omg-keys: wayland roundtrip failed while setting input region: {e}");
    }
}

/// Dispatch target for the one-off event queue above. `wl_region` has no events at all, so
/// this is never actually called -- it only needs to exist to satisfy `wayland_client`'s API.
struct State;

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
