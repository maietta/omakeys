//! The fullscreen grid overlay: a `gtk4-layer-shell` surface that draws the keyboard-shaped
//! hint grid, lets the user type a two-key code to warp the mouse there, then nudge/click
//! with vim-style keys.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::active_monitor;
use crate::atspi_scan;
use crate::config::{self, Settings, NUDGE_STEP_INCREMENT, NUDGE_STEP_MAX, NUDGE_STEP_MIN};
use crate::grid::{self, Cell};
use crate::hints::{self, HintTarget};
use crate::pointer::{VirtualPointer, BTN_LEFT, BTN_RIGHT};
use crate::{screencap, vision};

/// Dash length + gap length for the "marching ants" outline drawn around vision-detected
/// targets (buttons, panels, sections found by edge/contour detection rather than AT-SPI) --
/// see `dash_phase` and its tick timer in `Overlay::new()`. Vision detections get this
/// animated treatment (and a brighter color -- see `draw_hints()`) because they're the ones
/// that have repeatedly been hard to spot against busy app backgrounds; AT-SPI targets are
/// precise enough that a plain solid outline is already easy to find.
const VISION_DASH_PATTERN: [f64; 2] = [4.0, 3.0];

/// How many logical pixels `dash_phase` advances per animation tick -- see the tick timer in
/// `Overlay::new()`. Wrapped at the pattern's total length (dash + gap) so the phase doesn't
/// grow unbounded over a long-running daemon.
const DASH_PHASE_STEP: f64 = 0.6;

enum Mode {
    Hidden,
    /// No coarse key typed yet.
    TypingCoarse,
    /// Coarse key chosen; waiting for the fine key to pick the exact cell.
    TypingFine(char),
    /// A cell was picked; the pointer sits at (x, y) and can be nudged/clicked. `holding`
    /// tracks a Left-Shift-initiated click-and-drag: Shift down presses and holds the left
    /// button, arrow/hjkl movement while held drags (a real button-down click followed by
    /// motion is what any app interprets as a drag/selection), and Shift up releases it.
    Selected { x: f64, y: f64, holding: bool },
    /// Showing color-coded, keystroke-labeled boxes over buttons/text-fields/scrollables/
    /// terminals found via AT-SPI in windows on the focused monitor. `typed` accumulates
    /// label characters as they're typed, narrowing down to one target.
    Hints { targets: Vec<HintTarget>, typed: String },
    /// The settings/cheat-sheet menu, opened by holding Super past the long-press threshold
    /// (see `toggle_menu()`). `+`/`-` adjust `Settings::nudge_step` live; Escape closes.
    Menu,
}

struct State {
    mode: Mode,
    cells: Vec<Cell>,
    screen_w: f64,
    screen_h: f64,
    /// Connected only while the overlay is open (bound to the focused output).
    pointer: Option<VirtualPointer>,
    /// User-adjustable settings (currently just nudge step), loaded once at daemon startup
    /// and persisted to disk (`config::save`) whenever the menu changes one.
    settings: Settings,
    /// Advances continuously (see the tick timer in `Overlay::new()`) to animate a "marching
    /// ants" effect along vision-detected target outlines -- see `DASH_PHASE_STEP`.
    dash_phase: f64,
}

/// Layer-shell namespace shared by all 15 grid-region windows (see `GridRegionWindow`), kept
/// distinct from the main window's "omg-keys" namespace so a single Hyprland layer rule
/// (`no_screen_share` on this namespace -- see hyprland.lua) can exclude grid mode from
/// screen capture/recording without also hiding hint mode or the menu from it.
const GRID_REGION_NAMESPACE: &str = "omg-keys-grid";

/// One of the 15 small layer-shell windows tiling the screen into `grid::COARSE_KEYS`' 5x3
/// coarse regions, used only for `Mode::TypingCoarse`/`Mode::TypingFine` rendering.
///
/// Grid mode used to be drawn on the same single full-screen window as every other mode, but
/// Hyprland's `no_screen_share` layer-rule effect only excludes a layer surface's own bounds
/// from capture (confirmed via its source: the black-out rect is `CBox{REALPOS, REALSIZE}`,
/// i.e. that layer's own geometry) -- a full-screen window's "own bounds" is the whole
/// screen, so marking it would black out everything, not just our overlay. Splitting grid
/// mode into one small window per coarse region gives each one real, non-full-screen bounds,
/// so the layer rule can finally do what it's meant to: invisible to recordings, visible to
/// the human at the display. Hint mode and the menu still render on the original full-screen
/// window -- scattered/full-screen content doesn't tile onto a fixed 15-region grid the same
/// way, so they're left for a later pass (see HANDOFF.md).
#[derive(Clone)]
struct GridRegionWindow {
    window: gtk::ApplicationWindow,
    drawing_area: gtk::DrawingArea,
    /// This region's position within the 5x3 `grid::COARSE_KEYS` layout, used to translate
    /// `grid::Cell` coordinates (screen-absolute) into this window's local space.
    row: usize,
    col: usize,
}

pub struct Overlay {
    window: gtk::ApplicationWindow,
    drawing_area: gtk::DrawingArea,
    grid_windows: Vec<GridRegionWindow>,
    state: Rc<RefCell<State>>,
}

/// Build one of the 15 grid-region windows (see `GridRegionWindow`) -- same layer-shell setup
/// as the main window (overlay layer, click-through input region, no keyboard focus of its
/// own) but anchored Top+Left only, since its position and size are set later, dynamically,
/// once the target monitor's dimensions are known (see `Overlay::position_grid_windows`).
fn build_grid_window(
    app: &gtk::Application,
    state: &Rc<RefCell<State>>,
    coarse: char,
    row: usize,
    col: usize,
) -> GridRegionWindow {
    let window = gtk::ApplicationWindow::builder().application(app).decorated(false).build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(GRID_REGION_NAMESPACE);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    window.connect_realize(|window| crate::input_region::make_surface_click_through(window));

    let drawing_area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    window.set_child(Some(&drawing_area));

    {
        let state = state.clone();
        drawing_area.set_draw_func(move |_, cr, width, height| {
            let s = state.borrow();
            draw_grid_region(cr, width as f64, height as f64, &s, coarse, row, col);
        });
    }

    GridRegionWindow { window, drawing_area, row, col }
}

impl Overlay {
    pub fn new(app: &gtk::Application) -> Self {
        let state = Rc::new(RefCell::new(State {
            mode: Mode::Hidden,
            cells: Vec::new(),
            screen_w: 0.0,
            screen_h: 0.0,
            pointer: None,
            settings: config::load(),
            dash_phase: 0.0,
        }));

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("omg-keys grid")
            .decorated(false)
            .build();

        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("omg-keys");
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::None);

        // Make our own surface pointer-transparent -- omg-keys is 100% keyboard-driven, we
        // never need our own full-screen surface to receive real pointer input, and leaving
        // it receiving input (gtk4-layer-shell doesn't expose input-region control, so the
        // default is "whole surface") means it's the topmost thing at the cursor's position
        // for as long as it's mapped: every click sent through our own virtual pointer gets
        // swallowed before reaching the app underneath, and -- confirmed live, this is the
        // more serious half -- the *real* mouse can't move at all while the overlay is open.
        // Needs the surface to actually exist first, hence `connect_realize` rather than
        // doing this right here.
        window.connect_realize(|window| crate::input_region::make_surface_click_through(window));

        // Now that our own surface doesn't intercept pointer input (just above), the real
        // mouse can freely hover other windows while our overlay is open -- but Hyprland's
        // default `input:follow_mouse` setting continuously refocuses keyboard input to
        // whatever's under the pointer as it moves, which can steal keyboard focus away
        // from our on-demand grab entirely (confirmed live: the overlay got stuck open,
        // since Escape/the toggle key no longer reached it once focus moved elsewhere).
        // Asking users to change that global desktop setting just for this isn't the right
        // trade -- it affects every other app's focus behavior too -- so instead we just
        // reclaim the grab ourselves the moment we notice we've lost it while still
        // supposed to be open.
        {
            let state = state.clone();
            window.connect_notify_local(Some("is-active"), move |window, _| {
                // `try_borrow`, not `borrow`: this signal fires *synchronously* from inside
                // GTK/GDK calls like `set_visible(false)`, which several of our own handlers
                // call while still holding `state.borrow_mut()` (e.g. Escape) -- landing here
                // during one of those would be a conflicting borrow. `RefCell::borrow` panics
                // on that (confirmed live: crashed the whole daemon), so skip instead when
                // that happens; it's also simply the correct thing to do, not just a crash
                // workaround -- if *our own* code is mid-hide with the borrow held, this is
                // our own intentional close, not the focus-stolen-by-something-else case this
                // handler exists for, so there's nothing to reclaim.
                let Ok(s) = state.try_borrow() else { return };
                if !window.is_active() && !matches!(s.mode, Mode::Hidden) {
                    drop(s);
                    window.set_keyboard_mode(KeyboardMode::OnDemand);
                }
            });
        }

        // GTK's default theme paints an opaque window background; without this the whole
        // surface would be solid instead of letting the desktop show through underneath.
        let css = gtk::CssProvider::new();
        css.load_from_data("window, drawingarea { background-color: transparent; }");
        gtk::style_context_add_provider_for_display(
            &gtk4::prelude::WidgetExt::display(&window),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let drawing_area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        window.set_child(Some(&drawing_area));

        {
            let state = state.clone();
            drawing_area.set_draw_func(move |_, cr, width, height| {
                draw(cr, width as f64, height as f64, &state.borrow());
            });
        }

        let mut grid_windows = Vec::with_capacity(grid::COLS * grid::ROWS);
        for (row, keys) in grid::COARSE_KEYS.iter().enumerate() {
            for (col, &coarse) in keys.iter().enumerate() {
                grid_windows.push(build_grid_window(app, &state, coarse, row, col));
            }
        }

        // Drives the "marching ants" animation on vision-detected target outlines. Only
        // actually redraws while the overlay is showing something (checked inside, not by
        // starting/stopping the timer) -- simplest way to keep this correct across every
        // toggle path without threading timer start/stop through each one, and an idle
        // 25fps check-and-skip is negligible cost for a daemon that's normally just waiting
        // on IPC anyway.
        {
            let state = state.clone();
            let drawing_area = drawing_area.clone();
            let grid_windows = grid_windows.clone();
            glib::source::timeout_add_local(std::time::Duration::from_millis(40), move || {
                let mut s = state.borrow_mut();
                if matches!(s.mode, Mode::Hidden) {
                    return glib::ControlFlow::Continue;
                }
                let pattern_len: f64 = VISION_DASH_PATTERN.iter().sum();
                s.dash_phase = (s.dash_phase + DASH_PHASE_STEP) % pattern_len;
                drop(s);
                drawing_area.queue_draw();
                for gw in &grid_windows {
                    gw.drawing_area.queue_draw();
                }
                glib::ControlFlow::Continue
            });
        }

        {
            let state = state.clone();
            let drawing_area = drawing_area.clone();
            let window_for_handler = window.clone();
            let grid_windows_for_handler = grid_windows.clone();
            let key_controller = gtk::EventControllerKey::new();

            let state_for_release = state.clone();
            let drawing_area_for_release = drawing_area.clone();
            let window_for_release = window.clone();
            key_controller.connect_key_released(move |_, keyval, _keycode, _modifier| {
                handle_key_released(&state_for_release, keyval, &drawing_area_for_release, &window_for_release);
            });

            key_controller.connect_key_pressed(move |_, keyval, _keycode, modifier| {
                handle_key(&state, keyval, modifier, &drawing_area, &window_for_handler, &grid_windows_for_handler)
            });
            window.add_controller(key_controller);
        }

        Self { window, drawing_area, grid_windows, state }
    }

    /// Hide all 15 grid-region windows -- called whenever grid mode leaves
    /// `TypingCoarse`/`TypingFine` (a cell is picked, or the overlay closes), since they only
    /// ever draw content in those two modes.
    fn hide_grid_windows(&self) {
        hide_grid_windows(&self.grid_windows);
    }

    /// Position, size, and present all 15 grid-region windows to exactly tile `monitor`'s
    /// `screen_w` x `screen_h` area, matching `grid::build_grid`'s region math.
    fn position_grid_windows(&self, monitor: Option<&gdk4::Monitor>, screen_w: f64, screen_h: f64) {
        let region_w = (screen_w / grid::COLS as f64).round() as i32;
        let region_h = (screen_h / grid::ROWS as f64).round() as i32;
        for gw in &self.grid_windows {
            if let Some(monitor) = monitor {
                gw.window.set_monitor(monitor);
            }
            gw.window.set_default_size(region_w, region_h);
            gw.window.set_margin(Edge::Left, gw.col as i32 * region_w);
            gw.window.set_margin(Edge::Top, gw.row as i32 * region_h);
            gw.window.present();
        }
    }

    /// Toggle the overlay: show the grid if hidden, clear it if it's showing anything.
    pub fn toggle(&self) {
        let currently_hidden = matches!(self.state.borrow().mode, Mode::Hidden);
        if currently_hidden {
            self.show();
        } else {
            self.hide();
        }
    }

    /// Toggle AT-SPI hint mode: scan the focused monitor for buttons/text-fields/scrollables/
    /// terminals, label each with a short key sequence, and let the user type one to warp
    /// the pointer there and click it. A second call (or Escape) clears it.
    pub fn toggle_hints(&self) {
        let currently_hidden = matches!(self.state.borrow().mode, Mode::Hidden);
        if !currently_hidden {
            self.hide();
            return;
        }

        let (output_name, _monitor, _monitor_x, _monitor_y, w, h) = self.present_on_focused_monitor();

        let pointer = match VirtualPointer::new(output_name.as_deref()) {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("omg-keys: failed to create virtual pointer: {e}");
                None
            }
        };
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);

        {
            let mut s = self.state.borrow_mut();
            s.screen_w = w;
            s.screen_h = h;
            s.pointer = pointer;
        }

        // AT-SPI has no global desktop coordinate space under Wayland, so we scope the scan
        // to windows on the focused monitor (matched by title) and translate their element
        // coordinates into this monitor's local space using Hyprland's window positions --
        // see atspi_scan.rs. The vision pipeline below reuses the same window list to skip
        // regions that don't actually fall inside any window (empty desktop/wallpaper).
        let windows = match active_monitor::focused_monitor_windows() {
            Ok(windows) => windows,
            Err(e) => {
                log::warn!("omg-keys: could not list windows on the focused monitor ({e})");
                Vec::new()
            }
        };

        // Vision detection (screenshot + edge/contour analysis) is real CPU work, so it runs
        // on a background thread rather than blocking the glib main loop -- same handoff
        // pattern as the IPC listener in ipc.rs. It's kicked off concurrently with the async
        // AT-SPI scan below, not after it, so the two don't add latency serially.
        let (vision_tx, vision_rx) = async_channel::bounded(1);
        {
            let output_name = output_name.clone();
            let windows_for_vision = windows.clone();
            std::thread::spawn(move || {
                let regions = output_name
                    .as_deref()
                    .and_then(|name| match screencap::capture_output_gray(name) {
                        Ok(capture) => Some(vision::detect_regions(&capture, &windows_for_vision)),
                        Err(e) => {
                            log::warn!(
                                "omg-keys: screenshot capture failed ({e}), skipping vision hints"
                            );
                            None
                        }
                    })
                    .map(|regions| vision::filter_to_windows(regions, &windows_for_vision))
                    .unwrap_or_default();
                let _ = vision_tx.send_blocking(regions);
            });
        }

        let state = self.state.clone();
        let drawing_area = self.drawing_area.clone();
        glib::spawn_future_local(async move {
            let elements = match atspi_scan::scan_interactive_elements(&windows).await {
                Ok(elements) => elements,
                Err(e) => {
                    log::error!("omg-keys: AT-SPI scan failed: {e}");
                    Vec::new()
                }
            };
            let vision_regions = vision_rx.recv().await.unwrap_or_default();

            let atspi_count = elements.len();
            let vision_count = vision_regions.len();
            let targets = hints::assign_labels(elements, vision_regions);
            log::info!(
                "omg-keys: found {atspi_count} AT-SPI elements + {vision_count} vision \
                 regions ({} after merging) on the focused monitor",
                targets.len()
            );
            if log::log_enabled!(log::Level::Debug) {
                for t in &targets {
                    let (x, y, w, h) = t.geometry();
                    let src = if matches!(t.source, hints::Source::Vision(_)) { "vision" } else { "atspi" };
                    log::debug!("omg-keys: target [{}] {src} x={x} y={y} w={w} h={h}", t.label);
                }
            }
            let mut s = state.borrow_mut();
            s.mode = Mode::Hints { targets, typed: String::new() };
            drop(s);
            drawing_area.queue_draw();
        });
    }

    /// Toggle the settings/cheat-sheet menu (opened by a genuine Super long-press -- see
    /// `ipc::Command::ToggleMenu`'s doc comment). No pointer needed, unlike the other modes:
    /// the menu doesn't move the cursor or click anything.
    pub fn toggle_menu(&self) {
        let currently_hidden = matches!(self.state.borrow().mode, Mode::Hidden);
        if !currently_hidden {
            self.hide();
            return;
        }

        let _ = self.present_on_focused_monitor();
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);

        let mut state = self.state.borrow_mut();
        state.mode = Mode::Menu;
        drop(state);
        self.drawing_area.queue_draw();
    }

    /// Resolve the focused monitor, pin the layer surface to it, and present the window.
    /// Returns the output's connector name (if resolved), the resolved `gdk4::Monitor` itself
    /// (so callers that need to pin *other* windows to the same monitor -- see
    /// `position_grid_windows` -- don't have to re-resolve it), and its global
    /// `(x, y, width, height)` in logical pixels.
    fn present_on_focused_monitor(&self) -> (Option<String>, Option<gdk4::Monitor>, f64, f64, f64, f64) {
        let output_name = match active_monitor::focused_output_name() {
            Ok(name) => Some(name),
            Err(e) => {
                log::warn!("omg-keys: could not determine focused monitor ({e})");
                None
            }
        };

        let monitor = output_name.as_deref().and_then(find_gdk_monitor);
        let (x, y, w, h) = if let Some(monitor) = &monitor {
            self.window.set_monitor(monitor);
            let geom = monitor.geometry();
            (geom.x() as f64, geom.y() as f64, geom.width() as f64, geom.height() as f64)
        } else {
            (0.0, 0.0, self.window.width().max(1) as f64, self.window.height().max(1) as f64)
        };

        self.window.present();
        (output_name, monitor, x, y, w, h)
    }

    fn show(&self) {
        let (output_name, monitor, _x, _y, w, h) = self.present_on_focused_monitor();
        self.position_grid_windows(monitor.as_ref(), w, h);

        let pointer = match VirtualPointer::new(output_name.as_deref()) {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("omg-keys: failed to create virtual pointer: {e}");
                None
            }
        };
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);

        let mut state = self.state.borrow_mut();
        state.screen_w = w;
        state.screen_h = h;
        state.cells = grid::build_grid(w, h);
        state.mode = Mode::TypingCoarse;
        state.pointer = pointer;
        drop(state);
        self.drawing_area.queue_draw();
        for gw in &self.grid_windows {
            gw.drawing_area.queue_draw();
        }
    }

    fn hide(&self) {
        let mut state = self.state.borrow_mut();
        // See the matching comment in `handle_key`'s Escape handling -- a held button must
        // be released before the pointer is dropped, or it's left stuck "pressed".
        if let Mode::Selected { holding: true, .. } = state.mode {
            if let Some(p) = state.pointer.as_mut() {
                let _ = p.release(BTN_LEFT);
            }
        }
        state.mode = Mode::Hidden;
        state.pointer = None;
        drop(state);
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.set_visible(false);
        self.hide_grid_windows();
    }
}

/// Find the `gdk4::Monitor` whose connector name (e.g. "DP-3") matches `name`.
fn find_gdk_monitor(name: &str) -> Option<gdk4::Monitor> {
    let display = gdk4::Display::default()?;
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let monitor = monitors.item(i)?.downcast::<gdk4::Monitor>().ok()?;
        if monitor.connector().as_deref() == Some(name) {
            return Some(monitor);
        }
    }
    None
}

/// Briefly hide our own layer-shell surface, run `action` (a single discrete pointer button
/// event -- press, click, or release), then immediately reshow it. Our own full-screen
/// surface is otherwise still the topmost thing at the cursor's position when a button
/// event is processed, and swallows it before it ever reaches the app underneath --
/// confirmed live (landing on a terminal and clicking/typing did nothing at all until this
/// was added). Only the button event itself needs this treatment: once it's delivered to
/// the real target, Wayland's implicit pointer grab keeps routing every subsequent motion
/// and the matching release to that same target regardless of what's on top by then, so
/// ordinary nudging (`move_to` while the overlay stays visible) doesn't need it too.
fn click_through(s: &mut State, window: &gtk::ApplicationWindow, action: impl FnOnce(&mut VirtualPointer)) {
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_visible(false);
    if let Some(p) = s.pointer.as_mut() {
        std::thread::sleep(std::time::Duration::from_millis(20));
        action(p);
    }
    window.set_visible(true);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
}

/// Hide all 15 grid-region windows (see `GridRegionWindow`) -- the free-function twin of
/// `Overlay::hide_grid_windows`, for the key-handling free functions below that don't have
/// `&self`.
fn hide_grid_windows(grid_windows: &[GridRegionWindow]) {
    for gw in grid_windows {
        gw.window.set_visible(false);
    }
}

fn handle_key(
    state: &Rc<RefCell<State>>,
    keyval: gdk4::Key,
    modifier: gdk4::ModifierType,
    drawing_area: &gtk::DrawingArea,
    window: &gtk::ApplicationWindow,
    grid_windows: &[GridRegionWindow],
) -> glib::Propagation {
    if keyval == gdk4::Key::Escape {
        let mut s = state.borrow_mut();
        // A held button must be released before the pointer is dropped, or it's left stuck
        // "pressed" in the compositor with nothing left to ever send the matching release.
        if let Mode::Selected { holding: true, .. } = s.mode {
            if let Some(p) = s.pointer.as_mut() {
                let _ = p.release(BTN_LEFT);
            }
        }
        s.mode = Mode::Hidden;
        s.pointer = None;
        drop(s);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_visible(false);
        hide_grid_windows(grid_windows);
        return glib::Propagation::Stop;
    }

    let ch = keyval.to_unicode();
    let mut s = state.borrow_mut();

    // Hint-mode selection is handled separately: completing a label needs to mutate
    // `mode` and `pointer` together, which is awkward to interleave with the exhaustive
    // match below, so it gets its own borrow-scoped helper.
    if matches!(s.mode, Mode::Hints { .. }) {
        handle_hint_key(&mut s, keyval, ch, window);
        drop(s);
        drawing_area.queue_draw();
        return glib::Propagation::Stop;
    }

    let (screen_w, screen_h) = (s.screen_w, s.screen_h);

    match s.mode {
        Mode::Hidden => {}

        // Handled above, before this match.
        Mode::Hints { .. } => {}

        Mode::TypingCoarse => {
            if let Some(c) = ch {
                if grid::is_coarse_key(c) {
                    s.mode = Mode::TypingFine(c);
                }
            }
        }

        Mode::TypingFine(coarse) => {
            // h/j/k/l and the arrows are reserved for movement, never fine-picking: picking
            // a coarse region is enough on its own to start nudging immediately from its
            // center, without needing a second, separate fine-key press first. This makes
            // them entirely unreachable as fine-pick letters (those specific 4 of the 15
            // sub-cells in every region can no longer be addressed by a direct 2-key code --
            // reaching them now takes a coarse pick plus a nudge instead), which is exactly
            // the point: freehand nudging replaces them, deliberately.
            let nudge_step = s.settings.nudge_step;
            let movement = match keyval {
                gdk4::Key::h | gdk4::Key::Left => Some((-nudge_step, 0.0)),
                gdk4::Key::l | gdk4::Key::Right => Some((nudge_step, 0.0)),
                gdk4::Key::k | gdk4::Key::Up => Some((0.0, -nudge_step)),
                gdk4::Key::j | gdk4::Key::Down => Some((0.0, nudge_step)),
                _ => None,
            };
            if let Some((dx, dy)) = movement {
                if let Some((cx, cy)) = grid::coarse_region_center(&s.cells, coarse) {
                    let x = (cx + dx).max(0.0).min(screen_w);
                    let y = (cy + dy).max(0.0).min(screen_h);
                    if let Some(p) = s.pointer.as_mut() {
                        let _ = p.move_to(x, y, screen_w, screen_h);
                    }
                    s.mode = Mode::Selected { x, y, holding: false };
                    hide_grid_windows(grid_windows);
                }
            } else if let Some(c) = ch {
                if grid::is_fine_key(c) {
                    if let Some(cell) = grid::find_cell(&s.cells, coarse, c) {
                        let (x, y) = cell.center();
                        if let Some(p) = s.pointer.as_mut() {
                            let _ = p.move_to(x, y, screen_w, screen_h);
                        }
                        s.mode = Mode::Selected { x, y, holding: false };
                        hide_grid_windows(grid_windows);
                    }
                }
            }
        }

        Mode::Selected { x, y, holding } => {
            let nudge_step = s.settings.nudge_step;
            let mut new_pos = (x, y);
            let mut new_holding = holding;
            let mut handled = true;
            match keyval {
                gdk4::Key::h | gdk4::Key::Left => new_pos.0 = (x - nudge_step).max(0.0),
                gdk4::Key::l | gdk4::Key::Right => new_pos.0 = (x + nudge_step).min(screen_w),
                gdk4::Key::k | gdk4::Key::Up => new_pos.1 = (y - nudge_step).max(0.0),
                gdk4::Key::j | gdk4::Key::Down => new_pos.1 = (y + nudge_step).min(screen_h),
                gdk4::Key::space => {
                    let button = if modifier.contains(gdk4::ModifierType::SHIFT_MASK) {
                        BTN_RIGHT // approximates "alt click" as a right-click
                    } else {
                        BTN_LEFT
                    };
                    click_through(&mut s, window, |p| {
                        let _ = p.click(button);
                    });
                }
                // Left Shift down starts a click-and-drag: press and hold the left button
                // here, then every subsequent nudge below moves the pointer *while held*,
                // which is exactly what a real click-drag looks like to any app -- see
                // `handle_key_released` for the matching Shift-up that releases it.
                gdk4::Key::Shift_L if !holding => {
                    click_through(&mut s, window, |p| {
                        let _ = p.press(BTN_LEFT);
                    });
                    new_holding = true;
                }
                // Typing a regular printable character (not one of the reserved keys
                // matched above) means "I'm done positioning -- click here and let me
                // type": click to focus (the only way most apps actually give a text field
                // keyboard focus -- cursor position alone doesn't), close the overlay so
                // keyboard focus returns to the app, then forward this same character so
                // it isn't lost to the overlay's own key capture. Space stays the explicit
                // "click but keep the overlay open to keep positioning" action; mid-drag
                // (`holding`) is excluded since a stray letter there is far more likely a
                // slip than an intentional "I'm done".
                //
                // Hide *before* clicking, not after -- our own full-screen layer-shell
                // surface is otherwise still the topmost thing at that screen position when
                // the click is processed, and swallows it before it ever reaches the target
                // (confirmed live: landing on a terminal and typing did nothing at all until
                // this was reordered).
                _ if !holding && ch.is_some() => {
                    s.mode = Mode::Hidden;
                    window.set_keyboard_mode(KeyboardMode::None);
                    window.set_visible(false);
                    if let Some(p) = s.pointer.as_mut() {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        let _ = p.click(BTN_LEFT);
                    }
                    s.pointer = None;
                    drop(s);
                    forward_typed_character(ch.unwrap());
                    drawing_area.queue_draw();
                    return glib::Propagation::Stop;
                }
                _ => handled = false,
            }
            if handled {
                if new_pos != (x, y) {
                    if let Some(p) = s.pointer.as_mut() {
                        let _ = p.move_to(new_pos.0, new_pos.1, screen_w, screen_h);
                    }
                }
                s.mode = Mode::Selected { x: new_pos.0, y: new_pos.1, holding: new_holding };
            }
        }

        Mode::Menu => {
            let delta = match keyval {
                gdk4::Key::plus | gdk4::Key::equal | gdk4::Key::KP_Add => NUDGE_STEP_INCREMENT,
                gdk4::Key::minus | gdk4::Key::KP_Subtract => -NUDGE_STEP_INCREMENT,
                _ => 0.0,
            };
            if delta != 0.0 {
                s.settings.nudge_step =
                    (s.settings.nudge_step + delta).clamp(NUDGE_STEP_MIN, NUDGE_STEP_MAX);
                if let Err(e) = config::save(&s.settings) {
                    log::warn!("omg-keys: failed to save settings: {e}");
                }
            }
        }
    }

    drop(s);
    drawing_area.queue_draw();
    for gw in grid_windows {
        gw.drawing_area.queue_draw();
    }
    glib::Propagation::Stop
}

/// Forward a single character to whatever now has keyboard focus, via `wtype` (a Wayland
/// virtual-keyboard client) -- used when the user starts typing mid-grid-positioning (see
/// the `Mode::Selected` catch-all above), so the very character that triggered "auto-focus
/// and close" isn't itself lost to the overlay's own key capture. A short delay first gives
/// the compositor a moment to actually hand keyboard focus back to the target window after
/// `set_visible(false)`, rather than racing it. Requires `wtype` on PATH; logs and gives up
/// otherwise rather than failing the close/focus that already happened -- losing one
/// character is a much smaller problem than the overlay refusing to close.
fn forward_typed_character(c: char) {
    std::thread::sleep(std::time::Duration::from_millis(40));
    if let Err(e) = std::process::Command::new("wtype").arg(c.to_string()).status() {
        log::warn!("omg-keys: failed to forward typed character via wtype ({e}) -- is wtype installed?");
    }
}

/// Releasing Left Shift ends a click-and-drag started in `handle_key` -- release the left
/// button that was pressed and held there, at wherever the cursor ended up after any
/// nudging while held. A no-op outside `Mode::Selected` or when nothing is currently held.
fn handle_key_released(
    state: &Rc<RefCell<State>>,
    keyval: gdk4::Key,
    drawing_area: &gtk::DrawingArea,
    window: &gtk::ApplicationWindow,
) {
    if keyval != gdk4::Key::Shift_L {
        return;
    }
    let mut s = state.borrow_mut();
    if let Mode::Selected { x, y, holding: true } = s.mode {
        click_through(&mut s, window, |p| {
            let _ = p.release(BTN_LEFT);
        });
        s.mode = Mode::Selected { x, y, holding: false };
        drop(s);
        drawing_area.queue_draw();
    }
}

/// Handle one keystroke while in hint mode: Backspace undoes a typed character, any other
/// key extends the typed prefix if some target's label still matches it (ignored otherwise),
/// and completing a full label warps the pointer there, clicks it, and closes the overlay.
fn handle_hint_key(s: &mut State, keyval: gdk4::Key, ch: Option<char>, window: &gtk::ApplicationWindow) {
    if keyval == gdk4::Key::BackSpace {
        if let Mode::Hints { typed, .. } = &mut s.mode {
            typed.pop();
        }
        return;
    }

    let Some(c) = ch else { return };
    if !grid::is_hint_key(c) {
        return;
    }

    // Read-only pass first: figure out the new typed prefix and whether it completes a
    // label, without holding a borrow into `s.mode` past this block (completing a label
    // needs to mutate `s.mode` and `s.pointer` together afterward).
    let (candidate, full_match_center) = {
        let Mode::Hints { targets, typed } = &s.mode else { return };
        let candidate = format!("{typed}{c}");
        if !targets.iter().any(|t| t.label.starts_with(&candidate)) {
            return; // dead-end keystroke -- ignore it, same as the grid modes do
        }
        let center = targets.iter().find(|t| t.label == candidate).map(|t| t.center());
        (candidate, center)
    };

    if let Mode::Hints { typed, .. } = &mut s.mode {
        *typed = candidate;
    }

    if let Some((cx, cy)) = full_match_center {
        let (screen_w, screen_h) = (s.screen_w, s.screen_h);
        // Move while still visible (harmless -- motion doesn't hit-test against surfaces the
        // way a button click does), but hide *before* clicking, not after. Our own
        // full-screen layer-shell surface is otherwise still the topmost thing at that
        // screen position when the click is processed, and swallows it -- confirmed live,
        // the target never actually got focused. `KeyboardMode::None` first also drops our
        // keyboard grab before the click, so whatever we clicked can pick up focus cleanly.
        if let Some(p) = s.pointer.as_mut() {
            let _ = p.move_to(cx, cy, screen_w, screen_h);
        }
        s.mode = Mode::Hidden;
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_visible(false);
        if let Some(p) = s.pointer.as_mut() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = p.click(BTN_LEFT);
        }
        s.pointer = None;
    }
}

fn draw(cr: &cairo::Context, width: f64, height: f64, state: &State) {
    match &state.mode {
        Mode::Hidden => {}

        // Nothing drawn on the main window: grid mode now renders on 15 separate small
        // windows tiling the screen (see `GridRegionWindow`/`draw_grid_region`), so each can
        // be individually excluded from screen capture via Hyprland's `no_screen_share`
        // layer rule -- a full-screen window's "own bounds" for that rule is the whole
        // screen, so it can't be selectively excluded on its own.
        Mode::TypingCoarse | Mode::TypingFine(_) => {}

        // Nothing drawn: the overlay is fully invisible once a cell is picked, so only
        // the real cursor (already warped there) shows through. Keys are still captured
        // for nudging/clicking until Escape or the trigger key closes the overlay.
        Mode::Selected { .. } => {}

        Mode::Hints { targets, typed } => {
            if targets.is_empty() {
                // Silent "found nothing" is indistinguishable from "broken" -- e.g. the
                // focused app may just not support AT-SPI at all (Electron apps without
                // accessibility force-enabled, GTK4 apps with the position bug -- see
                // atspi_scan.rs). Say so instead of showing nothing.
                draw_no_hints_message(cr, width, height);
            } else {
                draw_hints(cr, targets, typed, width, height, state.dash_phase);
            }
        }

        Mode::Menu => {
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.75);
            let _ = cr.paint();
            draw_menu(cr, width, height, state.settings.nudge_step);
        }
    }
}

fn draw_no_hints_message(cr: &cairo::Context, width: f64, _height: f64) {
    let text = "omg-keys: no hintable elements found here — try Right Shift/Super for grid mode instead — Esc to close";
    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(15.0);
    if let Ok(extents) = cr.text_extents(text) {
        let padding = 10.0;
        let box_w = extents.width() + padding * 2.0;
        let box_h = extents.height() + padding * 2.0;
        let box_x = (width - box_w) / 2.0;
        let box_y = 24.0;
        cr.set_source_rgba(0.05, 0.05, 0.05, 0.9);
        cr.rectangle(box_x, box_y, box_w, box_h);
        let _ = cr.fill();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        draw_centered_text(cr, text, width / 2.0, box_y + box_h / 2.0);
    }
}

/// Keybind reference shown in the settings menu, as (key, description) pairs.
const CHEAT_SHEET: &[(&str, &str)] = &[
    ("Right Shift / Super (tap)", "Open grid mode"),
    ("Control_R", "Open hint mode (AT-SPI/vision element hints)"),
    ("Super (hold)", "Open this menu"),
    ("<coarse><fine>", "Grid: pick a cell by its two-key code"),
    ("h j k l / arrows", "Nudge the cursor"),
    ("Space", "Click"),
    ("Shift + Space", "Right-click"),
    ("Left Shift (hold)", "Click-and-drag: hold, nudge to select, release to finish"),
    ("<label>", "Hints: type a target's label to warp + click it"),
    ("Backspace", "Undo the last typed character"),
    ("Escape", "Close the overlay"),
];

fn draw_menu(cr: &cairo::Context, width: f64, height: f64, nudge_step: f64) {
    let title = "omg-keys";
    let row_h = 26.0;
    let padding = 24.0;
    let title_h = 40.0;
    let setting_h = 34.0;
    let footer_h = 30.0;

    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(13.0);
    let key_col_w = CHEAT_SHEET
        .iter()
        .filter_map(|(key, _)| cr.text_extents(key).ok().map(|e| e.width()))
        .fold(0.0_f64, f64::max);

    let box_w = 620.0_f64.min(width - 80.0);
    let box_h = title_h + row_h * CHEAT_SHEET.len() as f64 + setting_h + footer_h + padding * 2.0;
    let box_x = (width - box_w) / 2.0;
    let box_y = ((height - box_h) / 2.0).max(20.0);

    rounded_rect(cr, box_x, box_y, box_w, box_h, 10.0);
    cr.set_source_rgba(0.07, 0.07, 0.09, 0.97);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.4, 0.7, 1.0, 0.6);
    let _ = cr.set_line_width(1.5);
    let _ = cr.stroke();

    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(20.0);
    draw_centered_text(cr, title, width / 2.0, box_y + title_h / 2.0 + padding / 2.0);

    let key_x = box_x + padding;
    let desc_x = box_x + padding + key_col_w + 24.0;
    let mut row_y = box_y + title_h + padding;

    cr.set_font_size(13.0);
    for &(key, desc) in CHEAT_SHEET {
        cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_source_rgba(0.4, 0.85, 1.0, 0.95);
        cr.move_to(key_x, row_y);
        let _ = cr.show_text(key);

        cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_source_rgba(0.9, 0.9, 0.9, 0.95);
        cr.move_to(desc_x, row_y);
        let _ = cr.show_text(desc);

        row_y += row_h;
    }

    // The one live-adjustable setting -- visually separated from the static reference above.
    row_y += 6.0;
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.move_to(box_x + padding, row_y - 14.0);
    cr.line_to(box_x + box_w - padding, row_y - 14.0);
    let _ = cr.stroke();

    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_source_rgba(1.0, 0.85, 0.2, 1.0);
    cr.move_to(key_x, row_y);
    let _ = cr.show_text("+ / -");

    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_source_rgba(0.9, 0.9, 0.9, 0.95);
    cr.move_to(desc_x, row_y);
    let _ = cr.show_text(&format!("Nudge speed: {nudge_step:.0}px per press"));

    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_source_rgba(0.6, 0.6, 0.6, 0.9);
    cr.set_font_size(12.0);
    draw_centered_text(cr, "Esc to close", width / 2.0, box_y + box_h - footer_h / 2.0);
}

/// Draw an outline + callout bubble over each hint target. Targets whose label no longer
/// matches what's been typed so far are dimmed down to a faint outline and lose their
/// bubble, so the remaining candidates stand out as the user narrows down to one. The
/// bubble sits just outside the target (above by default, below if too close to the
/// screen's top edge) connected by a short arrow, rather than sitting on top of the target,
/// so the button/field/etc underneath stays fully visible. The bubble renders the
/// already-typed prefix in one color and the remaining characters in another, so progress
/// is visible at a glance.
fn draw_hints(
    cr: &cairo::Context,
    targets: &[HintTarget],
    typed: &str,
    screen_w: f64,
    screen_h: f64,
    dash_phase: f64,
) {
    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(13.0);
    let typed_count = typed.chars().count();

    for target in targets {
        let (x, y, w, h) = target.geometry();
        let matches = target.label.starts_with(typed);
        let is_vision = matches!(target.source, hints::Source::Vision(_));

        // Vision-detected targets get their own bright, distinct color rather than sharing
        // AT-SPI's (dull, by comparison) `Category::Other` gray -- they're the ones that
        // have repeatedly been hard to spot against busy app content, so they get the
        // "marching ants" animated dash below too, on top of the brighter color.
        let (r, g, b) = if is_vision {
            (1.0, 0.15, 0.75)
        } else {
            match target.category() {
                atspi_scan::Category::Button => (0.2, 0.9, 0.3),
                atspi_scan::Category::TextField => (0.3, 0.6, 1.0),
                atspi_scan::Category::Scrollable => (1.0, 0.6, 0.1),
                atspi_scan::Category::Terminal => (0.8, 0.3, 1.0),
                atspi_scan::Category::Other => (0.7, 0.7, 0.7),
            }
        };

        cr.set_source_rgba(r, g, b, if matches { 0.95 } else { 0.15 });
        let _ = cr.set_line_width(2.0);
        if is_vision {
            let _ = cr.set_dash(&VISION_DASH_PATTERN, dash_phase);
        } else {
            let _ = cr.set_dash(&[], 0.0);
        }
        cr.rectangle(x, y, w, h);
        let _ = cr.stroke();
        let _ = cr.set_dash(&[], 0.0);

        if !matches {
            continue;
        }

        draw_label_bubble(cr, &target.label, typed_count, (x, y, w, h), (r, g, b), screen_w, screen_h);
    }
}

const BUBBLE_H: f64 = 20.0;
const BUBBLE_PAD_X: f64 = 6.0;
const BUBBLE_CHAR_W: f64 = 9.0;
const BUBBLE_GAP: f64 = 16.0;

/// Draw a rounded-rect label bubble just outside a target box, connected to it by a short
/// arrow, instead of covering the target itself.
fn draw_label_bubble(
    cr: &cairo::Context,
    label: &str,
    typed_count: usize,
    (x, y, w, h): (f64, f64, f64, f64),
    (r, g, b): (f64, f64, f64),
    screen_w: f64,
    screen_h: f64,
) {
    let bubble_w = BUBBLE_CHAR_W * label.chars().count() as f64 + BUBBLE_PAD_X * 2.0;
    let target_cx = x + w / 2.0;

    // Prefer above the target; flip below if there's not enough room above. Either way,
    // clamp to the screen -- a target tall enough to span nearly the whole screen (e.g. a
    // full-height structural section) has no room on *either* side by the naive check
    // above, and without this clamp the bubble -- along with the label needed to actually
    // select the target -- would render entirely off-screen (confirmed live: a full-height
    // vision section box at y=24..1080 flipped "below" to y=1096, past the 1080px screen).
    let above = y - BUBBLE_GAP - BUBBLE_H >= 4.0;
    let natural_y = if above { y - BUBBLE_GAP - BUBBLE_H } else { y + h + BUBBLE_GAP };
    let bubble_y = natural_y.clamp(2.0, (screen_h - BUBBLE_H - 2.0).max(2.0));
    let bubble_x = (target_cx - bubble_w / 2.0).clamp(2.0, (screen_w - bubble_w - 2.0).max(2.0));

    // Leader line + small arrowhead from the bubble's near edge to the target's edge.
    let arrow_from_y = if above { bubble_y + BUBBLE_H } else { bubble_y };
    let arrow_to_y = if above { y } else { y + h };
    let arrow_x = target_cx.clamp(bubble_x + 6.0, bubble_x + bubble_w - 6.0);
    cr.set_source_rgba(r, g, b, 0.9);
    let _ = cr.set_line_width(1.5);
    cr.move_to(arrow_x, arrow_from_y);
    cr.line_to(target_cx, arrow_to_y);
    let _ = cr.stroke();
    draw_arrowhead(cr, target_cx, arrow_to_y, if above { 1.0 } else { -1.0 }, (r, g, b));

    rounded_rect(cr, bubble_x, bubble_y, bubble_w, BUBBLE_H, 5.0);
    cr.set_source_rgba(0.05, 0.05, 0.05, 0.92);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(r, g, b, 0.9);
    let _ = cr.set_line_width(1.0);
    let _ = cr.stroke();

    let mut tx = bubble_x + BUBBLE_PAD_X;
    for (i, label_char) in label.chars().enumerate() {
        if i < typed_count {
            cr.set_source_rgba(1.0, 0.85, 0.2, 1.0); // typed prefix: bright yellow
        } else {
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0); // remaining: white
        }
        cr.move_to(tx, bubble_y + BUBBLE_H - 6.0);
        let _ = cr.show_text(&label_char.to_string());
        tx += BUBBLE_CHAR_W;
    }
}

fn draw_arrowhead(cr: &cairo::Context, px: f64, py: f64, dir: f64, (r, g, b): (f64, f64, f64)) {
    let size = 4.0;
    cr.set_source_rgba(r, g, b, 0.9);
    cr.move_to(px, py);
    cr.line_to(px - size, py - dir * size);
    cr.line_to(px + size, py - dir * size);
    cr.close_path();
    let _ = cr.fill();
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Draw one grid-region window's content: its coarse label (`TypingCoarse`), its fine
/// sub-grid if it's the chosen region (`TypingFine(coarse)` where `coarse` matches), or just
/// the dim background wash if some *other* region was chosen. `width`/`height` are this
/// window's own size, which -- since `position_grid_windows` sizes every window identically
/// to one coarse region -- doubles as that region's width/height for translating `Cell`
/// coordinates (screen-absolute) into this window's local space.
fn draw_grid_region(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    state: &State,
    coarse: char,
    row: usize,
    col: usize,
) {
    match &state.mode {
        Mode::TypingCoarse => {
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
            let _ = cr.paint();

            cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
            cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            cr.set_font_size(28.0);
            draw_centered_text(cr, &coarse.to_string(), width / 2.0, height / 2.0);
            cr.rectangle(0.0, 0.0, width, height);

            // Bright, animated "marching ants" outline -- same treatment as vision-detected
            // hint targets, for the same reason: a plain faint line is easy to lose against
            // busy content.
            cr.set_source_rgba(0.15, 0.95, 1.0, 0.85);
            let _ = cr.set_line_width(1.5);
            let _ = cr.set_dash(&VISION_DASH_PATTERN, state.dash_phase);
            let _ = cr.stroke();
            let _ = cr.set_dash(&[], 0.0);
        }

        Mode::TypingFine(chosen) if *chosen == coarse => {
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
            let _ = cr.paint();

            // This window's origin in screen-absolute coordinates, to translate `Cell`
            // positions (screen-absolute, shared across the whole grid) into local space.
            let region_x = col as f64 * width;
            let region_y = row as f64 * height;

            cr.set_source_rgba(0.3, 0.9, 1.0, 0.95);
            cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            cr.set_font_size(22.0);
            for cell in grid::cells_in_region(&state.cells, coarse) {
                let (cx, cy) = cell.center();
                // h/j/k/l no longer pick this specific cell (see `Mode::TypingFine`'s
                // handler -- they're reserved for movement instead), so labeling them here
                // would be misleading: draw the cell's outline as usual, but no letter.
                if !matches!(cell.label[1], 'h' | 'j' | 'k' | 'l') {
                    draw_centered_text(cr, &cell.label[1].to_string(), cx - region_x, cy - region_y);
                }
                cr.rectangle(cell.x - region_x, cell.y - region_y, cell.w, cell.h);
            }
            cr.set_source_rgba(0.15, 0.95, 1.0, 0.85);
            let _ = cr.set_line_width(1.5);
            let _ = cr.set_dash(&VISION_DASH_PATTERN, state.dash_phase);
            let _ = cr.stroke();
            let _ = cr.set_dash(&[], 0.0);
        }

        // Some other region was chosen -- just the dim wash, matching the main window's
        // full-screen dim tint from before this was split into per-region windows.
        Mode::TypingFine(_) => {
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
            let _ = cr.paint();
        }

        _ => {}
    }
}

fn draw_centered_text(cr: &cairo::Context, text: &str, cx: f64, cy: f64) {
    if let Ok(extents) = cr.text_extents(text) {
        cr.move_to(cx - extents.width() / 2.0 - extents.x_bearing(), cy - extents.height() / 2.0 - extents.y_bearing());
        let _ = cr.show_text(text);
    }
}
