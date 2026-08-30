# omg-keys — handoff notes

## Goal

A keyboard-driven mouse replacement for Hyprland/wlroots (Wayland), written in Rust:

1. **Grid overlay** — tap Right Shift or Super to show a fullscreen grid. Each cell is
   addressed by a two-key code that mirrors physical keyboard layout onto screen position
   (e.g. `a` = home row, left pinky = middle-left of screen; `p` = top row, right pinky =
   upper-right of screen). Typing a coarse key (left hand) narrows to a region, typing a
   fine key (right hand) picks the exact cell and warps the mouse there. Then:
   - `h`/`j`/`k`/`l` nudges the cursor
   - `space` left-clicks, `shift+space` right-clicks (approximates "alt-click")
   - `Escape` or tapping the trigger key again closes the overlay
   - The overlay should be visually unobtrusive: translucent while picking a cell, and
     **fully invisible** (no dimming, no crosshair) once a cell is selected, so only the
     real cursor shows.
2. **Accessibility hint mode** — scan the AT-SPI accessibility tree for buttons, text
   fields, scrollable views, and terminal focus areas on the active screen, and draw
   color-coded boxes over them, each labeled with a short key sequence (Vimium-style link
   hints). Typing a label's characters narrows down the matching targets (non-matches dim
   out); completing a label warps the pointer there, clicks it, and closes the overlay.
   This is the "find everything that's a button or text field, and let me jump straight to
   one" half of the original ask. **Must work across apps, not just the ones AT-SPI happens
   to support well** — AT-SPI alone hits a hard coverage ceiling (GTK4, Electron — see
   "AT-SPI/Wayland limitations"), so a screenshot-based vision fallback fills the gap for
   whatever AT-SPI can't see into. AT-SPI-sourced boxes are solid-outlined; vision-sourced
   ones are dashed (less confident — we know *something* is there, not what).

## Architecture

- `omg-keysd` (daemon, `omg-keys daemon`): owns the GTK4 + `gtk4-layer-shell` overlay
  window, runs the `gtk4::Application` main loop on the main thread. Should be started
  once per session (wired into `~/.config/hypr/autostart.lua`).
- `omg-keys toggle` / `omg-keys hints` / `omg-keys quit`: thin CLI clients that connect to
  a Unix socket and send one `ipc::Command` to the running daemon, then exit. These are
  what Hyprland keybinds `exec`.
- IPC: plain blocking `std::os::unix::net::UnixListener` on a background thread
  ([src/ipc.rs](src/ipc.rs)), forwarding decoded `Command`s to the glib main loop via an
  `async_channel`, drained with `glib::spawn_future_local` in
  [src/main.rs](src/main.rs).
- Mouse movement: [src/pointer.rs](src/pointer.rs), a **separate** raw `wayland-client`
  connection (not GTK's) using `wlr-virtual-pointer-unstable-v1`, bound to the specific
  focused output so coordinates are that output's own local logical pixels (no
  multi-monitor offset math needed). Confirmed correct via wl-kbptr's C source
  (`~/.cache/yay/wl-kbptr`).
- Focused monitor detection: [src/active_monitor.rs](src/active_monitor.rs) shells out to
  `hyprctl -j monitors` to find the focused output's connector name (e.g. `DP-3`), which
  `overlay.rs` matches against `gdk4::Display::monitors()` (`Monitor::connector()`) to pin
  the layer-shell window there via `set_monitor()`.
- Grid math: [src/grid.rs](src/grid.rs), pure/tested — `COARSE_KEYS`/`FINE_KEYS` 5x3
  layouts, `build_grid()`, `find_cell()`, `cells_in_region()`. Unit tests confirm the
  spatial mapping (`a` → middle-left, `p` → upper-right within its region).
- Overlay/state machine: [src/overlay.rs](src/overlay.rs) — `Mode` enum
  (`Hidden` / `TypingCoarse` / `TypingFine` / `Selected` / `Hints`), `State` struct, GTK
  window setup, cairo `draw()`, `handle_key()`.
- AT-SPI scanning: [src/atspi_scan.rs](src/atspi_scan.rs) — matches AT-SPI application
  frames to real Hyprland windows on the focused monitor by title
  (`active_monitor::FocusedWindow`), walks each matching frame via the `atspi` crate,
  classifies roles into `Category` (Button / TextField / Scrollable / Terminal / Other),
  returns `Element { x, y, w, h, category, role, name }` in coordinates local to the
  focused monitor (translated using each window's real position — AT-SPI itself has no
  global coordinate space under Wayland; see "AT-SPI/Wayland limitations" below).
- Screenshot capture: [src/screencap.rs](src/screencap.rs) — **another separate** raw
  `wayland-client` connection (same pattern as `pointer.rs`), using
  `wlr-screencopy-unstable-v1` bound at version 1 (so only the plain `wl_shm` buffer path is
  needed, not `linux-dmabuf`). Allocates an anonymous `memfd` (`rustix::fs::memfd_create`),
  mmaps it (`memmap2`), asks the compositor to copy the focused output's frame into it, then
  converts the raw `Argb8888`/`Xrgb8888` bytes to 8-bit grayscale for edge detection.
- Vision region detection: [src/vision.rs](src/vision.rs) — runs `imageproc::edges::canny`
  on the grayscale capture once, then traces contours (`imageproc::contours::find_contours`)
  for discrete controls and any panel that happens to have a fully closed border, with
  quality filters before dedup — size bounds, a flatness check (grayscale std-dev) for
  icon/button-scale boxes, a rectangularity check (shoelace-formula polygon area / bbox
  area) for panel-scale boxes. `filter_to_windows()` (called from `overlay.rs`, using
  `active_monitor::FocusedWindow`) drops anything not actually inside a real window. Dedup
  uses true IoU (`iou()`), not containment ratio — see the notes for why that distinction
  matters. This is a coarse, classification-free signal — "something rectangular is here" —
  by design; it exists to cover what AT-SPI structurally can't see, not to replace it. Has
  unit tests (`vision.rs`'s `#[cfg(test)] mod tests`) covering the filter/dedup logic
  against synthetic data, no live capture needed. **Does not** try to detect whole
  structural sections (file explorer/editor/chat/terminal panes) that lack a closed border —
  a divider-line-projection approach for that was built, fought with false positives across
  most of a session, and ultimately removed (see "Vision pipeline tuning notes" #5-#11); the
  user falls back to the separate plain grid mode for those, same tradeoff the `tine`
  project makes for AT-SPI-blind apps.
- Hint label rendering: `overlay.rs`'s `draw_hints()`/`draw_label_bubble()` — labels render
  as rounded-rect callout bubbles positioned just outside each target (above by default,
  below if too close to the screen's top edge, clamped horizontally to stay on-screen),
  connected by a short arrow, rather than sitting on top of the target — the actual
  button/field/icon underneath stays fully visible instead of being covered by the label.
  Confirmed via screenshot. `BUBBLE_GAP` was widened (7px → 16px) after the user found the
  original spacing too tight. Known simplification, **now actually observed** (not just
  theoretical): bubble placement doesn't do collision avoidance between neighboring bubbles,
  so a dense vertical stack of small targets close together (e.g. VS Code's activity bar,
  one icon roughly every 40px) gets visibly overlapping bubbles. Not yet fixed — see Next
  steps.
- Hint labeling/merging: [src/hints.rs](src/hints.rs) — merges AT-SPI `Element`s with
  vision `VisionRegion`s into `HintTarget { source: Source::Atspi(_) | Source::Vision(_),
  label }`, dropping vision regions that substantially overlap an AT-SPI element (AT-SPI's
  is more precise and has a real name/category). Labels reuse the grid's 30-key alphabet
  (`grid::hint_alphabet()`, i.e. `COARSE_KEYS` + `FINE_KEYS` flattened): single characters
  when ≤30 combined targets, uniformly 2 characters otherwise (never mixed lengths, so no
  label is ever a prefix of another). `overlay.rs`'s `handle_hint_key()` matches typed
  characters against these labels, narrowing candidates on each keystroke and, on a full
  match, warping the pointer to the target's center, left-clicking, and closing the overlay
  — this part doesn't care which source a target came from, it's all through
  `HintTarget::center()`.
- `toggle_hints()` in `overlay.rs` runs the vision capture+detection on a **background
  `std::thread`** (real CPU work — screenshot + Canny + contour finding — so it must not
  block the glib main loop) concurrently with the async AT-SPI scan, handed back via an
  `async_channel`, same cross-thread-to-glib pattern as the IPC listener in `ipc.rs`.

## Hyprland integration (already applied to this machine)

- `~/.config/hypr/autostart.lua`: `o.launch_on_start("<repo>/target/release/omg-keys daemon")`
- `~/.config/hypr/bindings.lua`:
  ```lua
  o.bind("SHIFT + SHIFT_R", "Toggle omg-keys grid", "<repo>/target/release/omg-keys toggle", { release = true })
  o.bind("SUPER + SUPER_L", "Toggle omg-keys grid", "<repo>/target/release/omg-keys toggle", { release = true })
  o.bind("CTRL + Control_R", "Toggle omg-keys hints", "<repo>/target/release/omg-keys hints", { release = true })
  ```
  (Omarchy's Hyprland config is Lua now, not the old `hyprland.conf` text format — see
  `hyprctl repl`/`hl.bind` docs at https://wiki.hypr.land/Configuring/Basics/Binds/. Note:
  `CTRL_R` is not a valid keysym name — it's `Control_R`, per XKB naming, unlike `SHIFT_R`/
  `SUPER_L` which happen to already be correct XKB names.)
- Config validated with `hyprctl reload` + `hyprctl configerrors` (clean).
- Daemon binary is currently started manually for testing
  (`/home/t14/Projects/omg-keys/target/release/omg-keys daemon &`); it will also
  autostart on next full Hyprland session start via the config above.

## What's done and verified working (live-tested on this machine via grim screenshots)

- Grid overlay renders correctly on the focused monitor, coarse → fine narrowing works,
  keyboard-shape-to-screen-position mapping confirmed both by unit test and visually.
- Mouse warps to the selected cell via the virtual pointer.
- Overlay is translucent while picking, fully invisible once a cell is selected.
- AT-SPI hint scan (`omg-keys hints`) is now verified end-to-end via screenshot: it finds
  every window on the focused monitor's active workspace (not just the focused one), matches
  each to its AT-SPI application frame by title, and draws boxes at the *correct* on-screen
  position for apps whose toolkit actually reports real element positions (confirmed with
  Firefox — 19 elements, boxes landed exactly on its toolbar buttons/tabs/scroll area). See
  "AT-SPI/Wayland limitations" below for which apps this does and doesn't work for, and the
  bugs list for how the coordinate math got there.
- Hint **selection** is verified end-to-end (via `wtype` to simulate keystrokes, since this
  runs headless in an agent session): typing a target's single-character label (e.g. `e`
  over Firefox's "+" new-tab button) warped the pointer there, clicked it — a real new tab
  opened — and closed the overlay, all in one step. Escape mid-typing cancels cleanly with
  no side effects.
- **Vision-based hint fallback is implemented and confirmed working via screenshot** against
  VS Code (Electron — one of the apps AT-SPI structurally cannot see into): a single hints
  toggle found "0 AT-SPI elements + 46-48 vision regions" and rendered dashed 2-character-
  labeled boxes over real UI — file tabs, sidebar file-tree, toolbar icons, panel tabs, chat
  message blocks — none of which existed before this session. This is the concrete "works
  across apps" requirement the whole vision direction was scoped for. Two things are *not*
  cleanly re-verified yet, both worth a clean re-check next session:
  - **List-like UI grouping**: the sidebar file tree got one big bounding box around the
    whole list rather than one box per file row (individual row separators aren't strong
    edges for Canny to isolate) — clicking it lands on the box's geometric center, not
    necessarily on a specific row. Icons/tabs/buttons were precise; lists were not.
  - **Selecting a vision-only target**: attempted via `wtype qh` against a VS Code activity-
    bar icon, but the *user* was independently testing the real keybind live at the same
    moment (visible as extra `ToggleHints` log entries with fresh scan counts right around
    the same timestamp), which makes that one attempt's log trail ambiguous about whether
    the click that landed was from the synthetic test or the user's own concurrent input.
    The selection code path itself is shared verbatim with the already-proven AT-SPI case
    (`HintTarget::center()` doesn't branch on `Source`), so it's very likely fine, but
    should get one clean, uncontested confirmation.
  - The 2-character-label path (only exercised when a window has >30 combined targets) *is*
    now exercised for real by the vision pipeline (46-48 regions on VS Code) and rendered
    correctly on screen, unlike before when it was untested.
- **Structural panel detection ("hint the whole editor/terminal/chat pane") was built,
  fought with false positives for most of a session, and ultimately removed** — see "Vision
  pipeline tuning notes" #5-#11 for the full arc (divider-line projection did briefly work
  for the original ask, but no pixel-projection heuristic tried could reliably tell a real
  divider apart from a coincidentally line-shaped bit of text content, and one attempt
  measured *inverted* on a real capture). Retired in favor of the pre-existing, unrelated
  plain grid mode as an explicit fallback for these cases, matching how the `tine` project
  (github.com/smythp/tine) handles the same AT-SPI-blind-app problem on Wayland. Current
  state: vision-based hints only cover discrete controls and closed-border panels via
  contour tracing (reliable, unaffected by this); whole structural sections without a
  closed border are not auto-hinted.

## Bugs found and fixed along the way

1. **`KeyboardMode::Exclusive` silently kills the layer surface** on this Hyprland version
   (0.56.2) when combined with anchoring to all 4 edges + `exclusive_zone(-1)` — the
   surface would render once then vanish from `hyprctl layers` a second or two later, no
   error logged. Fixed by using `KeyboardMode::OnDemand` instead, which still receives
   keyboard input immediately without requiring a click.
2. **GTK's default theme paints an opaque window background**, defeating transparency.
   Fixed with a `CssProvider` forcing `window, drawingarea { background-color: transparent; }`.
3. **`show()`/`toggle_hints()` block the glib main thread** for ~1-2s (spawning `hyprctl`
   + doing full Wayland connect/roundtrip synchronously), so there's a visible delay
   before the overlay appears. Not yet fixed — see Next steps.
4. **AT-SPI recursion bug**: the tree walker returned early whenever a node lacked
   `State::Showing`, but top-level Application/Frame objects usually don't carry that
   state at all (only real visible widgets do), so it always bailed at depth 0 and found
   zero elements everywhere. Fixed: `Showing` now only gates whether to *record* a node
   as hintable, not whether to keep recursing into its children.
5. **`NO_AT_BRIDGE=1` is set globally** in this user's session environment, which
   prevents GTK/Qt/Electron apps from publishing to AT-SPI at all, regardless of the
   `org.gnome.desktop.interface toolkit-accessibility` gsetting (which was `false` and has
   been set to `true`). Already-running apps will never show up in scans; only apps
   launched fresh with `NO_AT_BRIDGE` unset will register. **Unresolved product decision**:
   should this env var be unset globally (affects all apps, minor perf/privacy cost) or
   left as-is with hints only usable against apps the user explicitly relaunches?
   Note: for single-instance apps (Nautilus, most GNOME apps) relaunching the CLI often
   doesn't help anyway — the new process just forwards to the existing background service,
   which still has the old env. Full quit (`<app> -q`, or kill the process) + relaunch is
   needed to actually get a fresh process.
6. **AT-SPI has no global desktop coordinate space under Wayland.** `Component.GetExtents`
   was assumed to return absolute-screen coordinates (matching X11-era AT-SPI semantics);
   confirmed by direct inspection (`python3-gobject` + `Atspi`) that it actually returns
   coordinates relative to the app's *own window origin*. This silently broke the original
   design (filter-by-absolute-monitor-bounds) on any monitor that isn't at Hyprland offset
   `(0, 0)` — hints would report "found 0 elements" with no error. Fixed by matching each
   AT-SPI application frame to its real Hyprland window (by title) and adding that window's
   own monitor-local position (`hyprctl -j clients` → `at` minus the monitor's own `x, y`)
   to every element found inside it. See `active_monitor::FocusedWindow` and the module docs
   in `atspi_scan.rs`.
7. **GTK4 apps report `(0, 0)` for every element's position on Wayland**, regardless of
   where they actually are (width/height come through fine). Confirmed directly against
   Nautilus — every node from the frame down to leaf list items reported `x=0, y=0`. This is
   an upstream GTK4 Wayland accessibility-backend limitation, not fixable from here. Since
   `(0, 0)` is indistinguishable from a real element genuinely at the origin, such elements
   are dropped rather than rendered as a garbage pile in the corner (`atspi_scan.rs`).
   Practical effect: hints currently only work for non-GTK4 apps (Firefox confirmed working;
   GTK3/Qt apps likely also work since they predate this regression, untested here).
8. **Stale AT-SPI registrations hang instead of erroring.** A background process (in this
   case a Nautilus instance that had gone unresponsive) left a zombie entry in the AT-SPI
   registry; D-Bus calls against it never return (no peer left to reply, no error either) —
   this hung the *entire* scan indefinitely, leaving the layer-shell overlay stuck open and
   grabbing keyboard input, which briefly broke the user's ability to use their desktop
   normally mid-session. Fixed with a 1.5s per-app timeout (`with_timeout` in
   `atspi_scan.rs`, racing each app's scan against `glib::timeout_future` via
   `futures_lite`) so one stale entry can't wedge the whole thing.
9. **Hints originally scanned only the single focused window**, missing everything else
   visible on screen. Generalized to every *mapped* window on the focused monitor's active
   workspace (`active_monitor::focused_monitor_windows()`), each matched to its AT-SPI frame
   independently and given its own position offset (see bug 6) — verified working with two
   windows open side-by-side (VS Code + Firefox), hints only appeared for the one whose
   toolkit actually supports it (Firefox), at the correct position.

## AT-SPI/Wayland limitations (read before extending hints mode)

- **AT-SPI works today**: Firefox (and likely other Gecko/Qt/GTK3 apps — untested but not
  expected to hit the same GTK4 regression).
- **AT-SPI doesn't work at all**: GTK4 apps (Nautilus, GNOME Text Editor, etc. — position
  bug, see #7) and Electron apps that haven't been launched with accessibility force-enabled
  (VS Code, Spotify — they simply don't register on the AT-SPI bus at all; confirmed via
  `Atspi.get_desktop(0)` child list).
- **This is why the vision fallback exists** (`screencap.rs` + `vision.rs`, implemented this
  session — see Architecture and "What's done"): it's the only way to get hints on GTK4 and
  Electron apps, confirmed working on VS Code. AT-SPI stays the *precise* source where it
  works (real name/category, exact position); vision is the toolkit-agnostic catch-all —
  coarse, unclassified, and currently worse on list-like UIs (see "What's done" for the
  known grouping issue).

## Vision pipeline tuning notes (read before touching `vision.rs`'s filters)

The filter/dedup logic in `vision.rs` went through several real iterations this session
based on live user feedback against VS Code, each fixing a concretely observed problem —
worth understanding *why* before changing the constants again, since a couple of these are
non-obvious tradeoffs where the "obvious" fix breaks something else:

1. **Phantom hints on an empty (wallpaper-only) monitor.** Vision has no notion of "is
   there even a window here" on its own — a photo/gradient wallpaper produces real Canny
   edges just like a UI would. Fixed with `filter_to_windows()`: drop any region whose
   center isn't inside a real window's bounds (from `active_monitor::FocusedWindow`, now
   carrying `w`/`h` as well as position). This is applied in the `overlay.rs` vision thread,
   not inside `detect_regions()` itself, so `detect_regions()` stays testable/reusable
   without needing a live window list.
2. **A code minimap covered in false hints.** Its tiny syntax-colored bars are all
   individually small-but-qualifying contours. Added a grayscale std-dev ("flatness") check
   — real controls are close to a flat fill, a minimap is not. This *helped* but didn't
   fully eliminate minimap noise: some sub-patches of a minimap (e.g. a long comment
   rendered as one solid-colored bar) are locally flat even though the minimap as a whole
   isn't. A cleaner fix would need to recognize the minimap as a structural region (a tall
   thin strip at the editor's edge with unusually many small stacked boxes) rather than
   filtering box-by-box — not implemented.
3. **Real buttons (VS Code's "Claude"/"Opencode" buttons) missing despite neighboring
   buttons being found fine.** Root cause: the original dedup compared *containment ratio*
   (intersection / smaller box's area), intended to catch "same widget traced twice" (an
   outer border + an inner highlight line a couple pixels in, which are nearly the same
   rectangle). But a small button genuinely nested inside a larger toolbar/panel *also* has
   high containment relative to its own small area, so it was getting suppressed as if it
   were a duplicate of the panel. Fixed by switching to true IoU (intersection / union),
   which stays high for near-duplicates (nearly the same rectangle, so union ≈ either box)
   but drops to near-zero for a small box genuinely inside a much bigger one (union ≈ the
   big box, swamping the small intersection). Verified with unit tests
   (`iou_is_high_for_near_duplicate_boxes` / `iou_is_low_for_a_small_button_inside_a_large_panel`).
4. **Rounded-corner panel/region detection** (e.g. "hint the whole terminal pane as one
   target") was requested as a new capability. Added a *rectangularity* check (shoelace-
   formula polygon area from the traced contour, divided by its bounding box's area) — a
   real panel, sharp- or rounded-cornered, fills nearly all of its bounding box; a rounded
   corner only shaves off a small sliver. **First attempt applied this check to every
   contour and was too aggressive**: it also rejected small borderless icon-only buttons
   (VS Code's activity bar — Explorer/Search/Source-Control icons all lost their hints),
   because an icon *glyph* (a magnifying glass, a gear) is often not remotely
   rectangular even though it's a perfectly good button. Fixed by only requiring
   rectangularity for **panel-scale** boxes (`MIN_PANEL_SIZE = 150` in both dimensions),
   which also skip the flatness check entirely (a panel's interior is *supposed* to be
   visually busy — that's real content, not noise). Small/icon-scale boxes go back to just
   the flatness check, matching the pre-rectangularity behavior that was already working.
   **Contour-based panel detection was never confirmed working end-to-end** in this
   session — no contour-traced panel-scale box was observed live, most likely because VS
   Code's pane dividers don't produce a strong enough, *fully closed* Canny edge to trace a
   whole rectangle around a pane (a subtle divider on only one side of a pane doesn't close
   into a loop by itself). Superseded in practice by #5 below, but the contour path is still
   there and still applies `MIN_RECTANGULARITY` to any panel-scale contour that *does*
   happen to close (e.g. an app that draws an actual bordered panel/card).
5. **"Detect the big structural areas as a whole" (file explorer, editor, chat, terminal)**
   — the same session, immediately after #4's contour approach didn't pan out, the user
   proposed the fix directly: detect the *divider lines* themselves rather than requiring a
   closed contour around the whole panel. Implemented as `detect_dividers_as_grid()`: a
   column/row "projection profile" over the Canny edge map (sum edge-pixels per column and
   per row) — a cheap, axis-aligned-only stand-in for a full Hough line transform, which
   would be overkill since UI dividers are never drawn at an angle. Columns/rows whose edge
   count crosses a coverage threshold are real dividers; the window is then gridded between
   them (plus the window's own bounds closing the outer edge) into candidate panel regions.
   **Confirmed working live** via screenshot: found and correctly boxed VS Code's
   editor|chat-panel divider as a full-height line, after tuning `DIVIDER_COVERAGE` down
   from an initial guess of `0.5` to `0.35` based on diagnostic logging (kept, at debug
   level — see #6) showing the real divider only reached ~41% continuous coverage,
   apparently interrupted by chat-message-bubble borders crossing it. This is genuinely a
   different, complementary technique from contour tracing (structural line-following vs.
   closed-shape tracing), not just a retuned version of #4 — see `detect_regions()`'s doc
   comment for how the two combine. Deliberately over-segments irregular layouts (a divider
   search doesn't know a terminal-under-editor split doesn't extend under the sidebar too,
   so it can grid a spurious cell there) rather than under-detect, consistent with vision's
   "coarse, extra candidates are fine" philosophy elsewhere.
6. **Why the terminal pane specifically still wasn't showing up**, once #5 was live: a
   single window-wide divider scan is the wrong scope for a divider that's local to one
   sub-panel (a terminal's top border, measured against the *whole window's* width, may
   never clear the coverage threshold even though it's completely real — it was never
   meant to span the file-explorer sidebar too). Fixed by making `detect_dividers_as_grid`
   **recursive** (`detect_dividers_recursive`, `MAX_DIVIDER_DEPTH = 2`): after gridding at
   the window level, re-scan each resulting cell for finer dividers scoped to just that
   cell's own width/height. Verified with a unit test constructing exactly this scenario
   (`detect_dividers_as_grid_finds_a_divider_scoped_to_a_sub_panel`) before ever touching a
   live capture. **But recursion alone still didn't find VS Code's actual editor|terminal
   boundary** — investigated with direct pixel sampling (Python/PIL against a raw
   screenshot, not more threshold guessing) and found the real cause: in this theme, the
   editor and terminal panel share the *same background color* (`(17,26,19)` vs
   `(18,26,20)` — a 1-unit difference, i.e. noise, confirmed at five different x positions
   across the boundary). There is no rendered border and no color step there at all; the
   only visual cue a human uses is the small "Problems / Output / Debug Console / Terminal
   / Ports" tab-label row, which is text, not a line. **No amount of threshold tuning on
   this technique can find a boundary that isn't visually drawn** — this is a hard,
   confirmed dead end for edge/line-based detection specifically on this divider, not an
   unresolved tuning question. If it matters enough to chase further, the tab-label row
   itself would need a different, pattern-specific detector (e.g. OCR, or recognizing "a
   short horizontal run of small text labels" as its own signal) rather than more work on
   the line-projection approach.

7. **"Try much harder" to catch edge-to-edge lines** — the user pushed for a more
   aggressive divider signal: a line that spans nearly the whole window is a strong clue
   even if it's not dense/continuous. This produced a real improvement and then two
   dead-end refinement attempts, each disproven with real data rather than assumption --
   worth understanding the full arc before touching this code again:
   - **Span-based qualification** (`LineStats.first`/`.last`, the `qualifies()` OR-branch):
     a column/row now also qualifies if the distance between its first and last edge pixel
     covers ≥85% of the window (`DIVIDER_SPAN_COVERAGE`), with only a low minimum density
     required (`DIVIDER_MIN_DENSITY = 0.15`) to rule out two coincidental specks near
     opposite edges. This is a **kept, working improvement** — catches genuinely
     interrupted-but-edge-to-edge dividers that pure density missed.
   - **Local-peak check** (`is_local_peak`, `DIVIDER_PEAK_RATIO = 1.6`): span-qualification
     alone let a false positive through — a single line of terminal-rendered content at
     y=683 (not the real divider) passed density+span purely because it happened to
     stretch most of the window width. Added a check that a candidate must be
     meaningfully denser than the *average* of a band of rows/columns just outside its own
     width (`DIVIDER_PEAK_OFFSET_MIN/MAX = 5..25`, averaged rather than sampled at one
     point, since a single sample point turned out to land in text's own internal
     line-height rhythm by coincidence and falsely read as "neighbor is empty"). **Kept**,
     genuinely helps distinguish a spike from a broad band of similarly-dense
     neighbors -- but see next bullet, it did **not** fix the y=683 case specifically.
   - **Run-length check (tried, reverted)**: y=683 turned out to be an isolated line in
     otherwise-blank space, not a broad text band, so it was a genuine local peak by any
     density-based measure — the peak check couldn't touch it. Hypothesized that a real
     divider is one long continuous stroke while text is many short character-width
     strokes with gaps, and added a check requiring a long unbroken run of edge pixels
     (`LineStats.longest_run`, tracked incrementally during the same pixel pass). Measured
     directly against the real capture before trusting it (see the now-removed
     `temp_real_screenshot_check` test, and do this again rather than guess if revisiting):
     the *real* editor|chat divider's longest continuous run was only **65px / 6% of
     height** — it's chopped up far more finely by chat-bubble borders than assumed — while
     the y=683 false positive's longest run was **379px / 20% of width**, nearly 4x longer.
     Any run-length threshold that rejects the false positive also rejects the real
     divider; the signal is inverted for this case, not just imprecise. Reverted
     entirely — `LineStats` has no run-tracking fields anymore.
   - **Conclusion**: a terminal-rendered horizontal rule (or similar content that happens
     to draw a long line) and a real UI-chrome divider are pixel-statistically
     indistinguishable by density, span, peak-ratio, *or* run-length, individually or
     combined. This specific false-positive category — structural-*looking* lines that are
     actually content, not chrome — is a known, accepted limitation of the current
     approach, not an unsolved tuning question. A real fix would need information outside
     raw edge geometry (e.g. "is this inside a scrollable content pane" from window/widget
     structure, or OCR to confirm text presence) rather than another 1D-projection
     heuristic.
8. **"The drawn boxes don't line up with the actual findings" / "you're not seeing the
   main sections"** — investigated as a suspected coordinate bug first (it wasn't): spot
   checked specific logged target coordinates against fresh screenshots, cropped exactly
   at each one, in both a single-maximized-window scenario and a tiled two-window one
   (which exercises the `active_monitor.rs` window-offset math directly) — every single
   target checked, AT-SPI and vision alike, landed exactly where it should. The real issue
   was structural, not a coordinate bug: `detect_dividers_recursive` was gridding x-splits
   and y-splits together as one multiplied Cartesian product, so **any** spurious divider
   on either axis fragmented the *other*, cleaner axis's cells too. Confirmed live against
   VS Code: two real vertical dividers (editor|chat, sidebar|editor) coexisted with one
   spurious horizontal line (content crossing the row threshold, same failure mode as
   the y=683 case in #7) — and because of the Cartesian multiplication, that one bad row
   chopped *both* vertical panels in half, so neither the editor nor the chat panel was
   ever emitted as a single clean full-height box; the "boxes" the user was seeing were
   fragments, not the sections themselves.
   - **Fix, part 1**: emit full-height column strips and full-width row strips
     *independently* of the x*y grid (`had_x_dividers`/`had_y_dividers` computed before
     inserting the window's own boundaries, which otherwise makes "found nothing on this
     axis" indistinguishable from "found a real divider" once the boundary values are
     inserted). Confirmed live: three clean full-height boxes appeared (sidebar, editor,
     chat) for the first time.
   - **Fix, part 2**: that alone still produced two *bogus* full-width row strips (y=191,
     y=594) cutting straight across the sidebar/editor/chat boundary, visible as dashed
     lines running through unrelated code text and chat messages simultaneously. Root
     cause: a real top-level app window is essentially always organized as side-by-side
     panels first, not both a row split and a column split genuinely at once at the same
     level -- a "row" spanning the *entire* width when strong column dividers already
     exist is stitching together coincidentally-aligned content from otherwise-independent
     columns, not tracing a real boundary. Fixed by making row-strip emission conditional
     on `!had_x_dividers` -- row strips now only emitted when there's no competing column
     structure at that level. Horizontal dividers scoped to *one* column (the original
     terminal-under-editor motivation for recursion in #6) are unaffected, since those are
     found via the recursive call into that column specifically, where the column's own
     (narrower) width is what gets measured, not the whole window's.
   - Confirmed live with a before/after screenshot comparison: before, dashed lines
     crisscrossed the entire window in a grid pattern, cutting through source code and
     chat text alike; after, exactly three clean vertical section boundaries, matching
     what "the main sections of VS Code" actually looks like. Both fixes have unit tests
     (`detect_dividers_as_grid_keeps_a_clean_column_despite_a_spurious_row`, which asserts
     both that the real columns survive *and* that the bogus row strip is absent) built
     from the exact live scenario before the fix was trusted.

9. **Recursing into noisy grid-cell intersections instead of clean strips** — a follow-up
   to #8's fix: recursion (`detect_dividers_recursive`'s self-calls) originally still
   walked the full x*y grid-cell intersections, not just the clean column/row strips. Those
   intersection cells can be small enough that a single line of ordinary code or chat text
   trivially dominates them (satisfying density/span/peak all at once, purely because the
   "neighborhood" being compared against is tiny). Fixed by recursing only into each clean
   strip as a whole (`detect_dividers_recursive(edges, inset((x0, wy, w, wh)), ...)` for
   columns, symmetric for rows) — the grid-cell-intersection loop was removed entirely, not
   just deprioritized. This introduced a second bug: recursing into a strip immediately
   re-detects the divider that *created* that strip as if it were internal structure (the
   cut point is, by definition, sitting right at the strip's own edge), which then wrongly
   suppressed genuine dividers scoped to that strip. Fixed with `inset()` — shrink the
   recursed scan area by `RECURSE_INSET = DIVIDER_MERGE_GAP + 2 = 6` px on every side before
   searching it for further dividers. Confirmed via a failing test first (a duplicate
   sub-region was appearing at the strip's own boundary), then fixed.
10. **Divider-line flatness check (tried, reverted)** — the user reported a horizontal line
    still cutting through real VS Code comment text ("// Ask Hyprland which
    monitor/window...") even after #7-#9. Reused the `MAX_STD_DEV` flatness signal (already
    proven for widget/panel boxes) applied to divider *lines* specifically: sample the
    grayscale standard deviation of the original capture (not the edge map) along a
    candidate's own detected span (`LineStats.first..=.last`), on the theory that a real
    divider is close to one solid color end to end while a line of text is not
    (`DIVIDER_MAX_LINE_STD_DEV = 18.0`). This **did** kill the reported comment-line false
    positive when tested live — but broke the real editor|chat divider in the same test:
    measured directly (a temporary debug probe, not assumption), the genuine divider at
    x=1497 had **std_dev=29.9**, while a *different* false-positive text row (y=515, sparse
    chat/code text in two unrelated panels coincidentally aligned at the same height) had
    **std_dev=7.8-9.8** — lower than the real divider, not higher. The real divider's own
    column isn't flat (it crosses the tab bar and differently-styled editor vs. chat
    backgrounds along its length), while a *sparse* enough text row can look flatter than an
    actual line purely by sampling mostly-background pixels. This is the same shape of
    failure as #7's run-length revert — inverted for this case, not just imprecise — so it
    was fully reverted (not left disabled behind a flag): `DIVIDER_MAX_LINE_STD_DEV`,
    `col_is_flat`/`row_is_flat`, and the `gray`/`img_w` plumbing threaded through
    `detect_dividers_as_grid`/`detect_dividers_recursive` for it are all gone again. Density,
    span, peak-ratio, run-length, and now flatness have all been tried, individually, against
    real captures, and each either doesn't discriminate real-divider-vs-content-line or is
    actively inverted for at least one observed case. Treat this false-positive category (a
    structural-looking line that's actually text content) as a confirmed, accepted limit of
    1D edge-projection heuristics, not a threshold-tuning problem — see #7's conclusion,
    which still stands. A real fix needs a different kind of signal entirely (e.g. OCR to
    positively confirm "this is text", or reasoning about window/widget structure rather than
    raw pixels).

11. **Divider-line detection (`detect_dividers_as_grid`/`detect_dividers_recursive`,
    #5-#10 above) removed entirely.** After five different pixel-projection heuristics
    (density, span, peak-ratio, run-length, flatness) each either failed to discriminate a
    real divider from a coincidentally line-shaped bit of text/content, or measured
    *inverted* on a real capture (#7, #10), this stopped looking like a tuning problem and
    started looking like a fundamental limit of 1D edge-projection statistics for this
    discrimination. Researched how comparable projects solve the same problem (a Linux
    Wayland desktop-automation tool, `tine` — github.com/smythp/tine — built for driving a
    GNOME desktop from an AI agent, same AT-SPI-primary/fallback-for-blind-apps shape as
    this project): it doesn't try to auto-detect element boundaries in the AT-SPI-blind case
    at all. It falls back to a plain labeled coordinate grid overlaid on the screenshot and
    lets the human/agent point at a cell -- "not fancy, but it works." Adopted the same
    tradeoff here: `detect_dividers_as_grid`, `detect_dividers_recursive`, `LineStats`,
    `qualifies()`, `is_local_peak()`, `merge_adjacent()`, `inset()`, and all the
    `DIVIDER_*`/`RECURSE_INSET`/`MAX_DIVIDER_DEPTH` constants and their 9 dedicated tests are
    gone from `vision.rs`. `detect_regions()` no longer takes a `windows` parameter (it was
    only used to scope the divider scan). Structural sections that aren't traceable as a
    single *closed* contour (VS Code's file-explorer/editor/chat panes, which are usually
    just one drawn divider line with the rest of their "border" being the window edge) are no
    longer hinted automatically at all -- the user falls back to the pre-existing, unrelated
    plain grid mode (`Mode::TypingCoarse`, Right Shift/Super) for those, same as `tine`'s
    agent would. `draw_no_hints_message()` now says so directly ("try Right Shift/Super for
    grid mode instead") instead of just reporting zero elements. Verified live against VS
    Code post-removal: the comment-line and y=515 false positives from #7/#10 are both gone
    (there's no more divider-grid code path to produce them), the small icon/button-scale
    contour detections that were always reliable are untouched, and the separate grid-mode
    fallback still opens correctly side by side with hint mode.
    - **Next real option researched, not yet attempted**: `ocrs`
      (github.com/robertknight/ocrs) is a pure-Rust OCR engine (no Python/C++ dependency,
      uses the `rten` pure-Rust ML runtime) whose `OcrEngine::detect_words()` /
      `find_text_lines()` API exposes text-region *detection* as a separate, cheaper step
      before character recognition -- load only the detection model and skip recognition
      entirely if just "is there text here" bounding boxes are needed. This would be a
      genuinely different kind of signal from anything tried in #7-#10 (a *positive*
      "this is text" detector to exclude from divider candidacy, not another indirect
      pixel statistic to infer it) -- worth trying **if** structural panel-as-a-whole
      hinting is wanted back badly enough to justify the new dependency + model download;
      not attempted this session since the grid-mode fallback was the cheaper, already-
      validated-elsewhere fix actually implemented.

Current tuned constants (`vision.rs`): `MIN_SIZE = 14`, `MAX_SIZE_FRACTION = 0.9`,
`DEDUP_IOU = 0.7`, `MAX_STD_DEV = 45.0`, `MIN_RECTANGULARITY = 0.85`, `MIN_PANEL_SIZE = 150`
(only reachable via a closed contour now -- see #11). All picked from one iteration against
one app (VS Code) and one theme — see Next steps for tuning against more apps.

## Gotchas for whoever scripts `hyprctl` on this machine

This machine's Hyprland (Omarchy config, v0.56.2) parses `hyprctl dispatch` arguments as
**Lua**, not the classic space/comma syntax from upstream Hyprland docs. `hyprctl dispatch
workspace 9` and `hyprctl dispatch 'workspace(9)'` both fail to parse. What works is calling
through the `hl.dsp` namespace, e.g.:
```bash
hyprctl dispatch 'hl.dsp.window.close("0x56265c682e40")'
```
Plain dispatcher names error with "expected a dispatcher (e.g. hl.dsp.window.close())" —
that error message is the best in-situ documentation available for the correct form.

**`long_press` does not discriminate short vs. long presses of the same key.** Tested live:
bound `{ release = true }` and `{ long_press = true }` on the same key (`ALT + Alt_R`,
logging press+timestamp to a file) and had the user do a quick tap and a deliberate ~1s
hold. Both bindings fired together, microseconds apart, on *every* press regardless of
actual duration. So a "short tap → grid, long hold → hints" scheme on one physical key
(originally requested, along with "hold both Shift keys together" — also not supported, see
below) isn't achievable this way; ended up with two separate keys instead (Shift_R/Super_L
for grid, Control_R for hints). If this gets revisited, the `long_press` flag needs more
investigation (possibly a threshold config elsewhere, or this Hyprland build's behavior
differs from what's documented) before trying it again.

**Hyprland's bind engine can't require both physical Shift (or Ctrl/Alt/Super) keys held
together, as distinct from either one alone.** Confirmed via a live Hyprland maintainer
GitHub discussion: `bind` collapses left/right into one modifier bit (a bind on `SHIFT +
Shift_R` fires on *any* Shift-Shift_R combination, including Shift_R alone, since pressing
it satisfies its own modifier requirement). `binds` (plural) exists to make `Shift_L`/
`Shift_R` distinguishable from each other, but Omarchy's Lua wrapper only exposes `hl.bind`
(singular) — no `hl.binds` in the stub API (`/usr/share/hypr/stubs/hl.meta.lua`). A true
chord would need a kernel-level remapper (`keyd`) synthesizing a distinct key, which is a
bigger, separate system change.

## Next steps / open items

1. **Re-verify vision-target selection cleanly** (no concurrent live input this time) and
   confirm the file-tree-style list-grouping issue with a fresh screenshot comparison. Both
   described in detail under "What's done" above.
2. **Tune the vision pipeline against more apps.** Only tested live against VS Code so far.
   Canny thresholds (currently hardcoded `20.0, 50.0` in `vision.rs`) and the size/dedup/
   quality constants (`MIN_SIZE`, `MAX_SIZE_FRACTION`, `DEDUP_IOU`, `MAX_STD_DEV`,
   `MIN_RECTANGULARITY`, `MIN_PANEL_SIZE`) were all picked from iterating against one app
   (VS Code) and one theme, not swept — worth trying against a few more GTK4/Electron apps
   (GNOME Text Editor, Slack/Discord if installed, a terminal emulator) to see how well they
   generalize. See "Vision pipeline tuning notes" above for why each constant exists.
3. **List-like UI grouping** (see "What's done"): consider a follow-up pass that splits a
   large vision box into evenly-spaced sub-regions when it's tall/wide relative to typical
   row height, or leans on Canny's *internal* horizontal lines within the box (row
   separators) rather than just the outer contour, to approximate per-row targets.
4. **Bubble collision avoidance** (see "What's done" — now an observed problem, not just a
   theoretical one: VS Code's activity bar, one icon roughly every 40px, produces visibly
   overlapping label bubbles). `draw_label_bubble()` places each bubble independently with
   no awareness of its neighbors. Would need real layout logic — e.g. detect when two
   targets are close enough that their default above/below placement would collide, and
   offset one to the side, or increase leader-line length to route around the conflict.
5. **Structural panel-scale detection (divider-line-based) was removed** (see "Vision
   pipeline tuning notes" #11) after repeatedly failing to discriminate real dividers from
   line-shaped text content. Contour-based panel detection (#4 in the same notes) remains
   the only path to a whole-panel hint, and only catches panels with a genuinely *closed*
   border — a separate, likely lower-priority thing to revisit only if some app's panels
   turn out to have real closed borders contour tracing could catch. If whole-structural-
   section hinting is wanted back, the next real option (researched, not attempted) is
   `ocrs` (github.com/robertknight/ocrs, pure-Rust OCR/text-detection) as a *positive*
   text-region signal to exclude from divider candidacy — see #11's last bullet — rather
   than another indirect pixel-projection heuristic.
6. **Decide on `NO_AT_BRIDGE`** — unset globally in the session, or leave as opt-in per
   app. Lower priority now that vision covers Electron/GTK4 apps regardless of this setting
   (vision doesn't need AT-SPI bridging at all), but still relevant for AT-SPI's *precision*
   advantage on those apps if it were ever unset.
7. **Fix the ~1-2s show() delay**: move `active_monitor::focused_output_name()`
   (subprocess spawn) and `VirtualPointer::new()` (blocking Wayland roundtrips) off the
   glib main thread — e.g. spawn on a `std::thread` and post the result back via
   `async_channel` + `glib::spawn_future_local`, mirroring the IPC listener pattern
   already used in `ipc.rs` (and now also used for the vision capture thread).
8. **Concurrency bug** (elevated priority — the failure mode it describes was hit live
   this session, via a *different* trigger, and again during vision-target testing): `Mode`
   is a single shared field; if `ToggleGrid` and `ToggleHints` race (e.g. a hints scan is
   in-flight when the grid is toggled, or the user presses the hints keybind again while a
   scan from the previous press hasn't finished), the async completion handler
   unconditionally overwrites `state.mode`. Now that there are *two* concurrent async
   sources feeding one `toggle_hints()` call (the AT-SPI scan and the vision thread), plus
   the possibility of a second `toggle_hints()` call arriving before the first resolves,
   this is more likely to matter than when it was originally flagged. Consider tagging each
   async result with a generation counter and only applying it if nothing else has changed
   the mode since it was kicked off.
9. **Alt-click is only approximated as right-click** (`shift+space`). True Alt-modifier
   click would need `zwp-virtual-keyboard-manager-v1` (no ready Rust bindings found;
   Hyprland/wlroots supports the protocol per `/usr/include/hyprland/protocols/`). Add if
   the user wants literal Alt+click semantics.
10. Minor cleanup: `#[warn(dead_code)]` on `atspi_scan.rs`'s `Element::role`/`name`/`center()`
    (now genuinely unused since `hints.rs` reads geometry/category through `HintTarget`
    instead) — either wire up richer hint labels (e.g. show the accessible `name` as a
    tooltip, which would also be a natural place to surface vision regions' lack of a name)
    or silence with `#[allow(dead_code)]` if intentionally reserved for later.
11. No automated tests exist for `overlay.rs`, `pointer.rs`, `atspi_scan.rs`, `ipc.rs`, or
    `screencap.rs` — only `grid.rs` and (as of this session) `vision.rs` have unit tests.
    The rest need a live Wayland session so are harder to unit test, but consider at least a
    manual test checklist or a `--print-only` debug mode (see wl-kbptr's `--print-only` flag
    for prior art) to ease iteration without a full compositor round-trip each time.
12. Now that hints matches every window on the focused monitor (see bugs list), consider
    whether the *grid* overlay's monitor-only scope should extend similarly, or whether
    that's out of scope (grid warps the mouse via absolute monitor-local coordinates
    already, so it may not need this).
13. **Possible Phase 3** (not started, low priority): OCR on vision regions for name-based
    search instead of pure spatial hints, since vision targets currently have no name at all
    (unlike AT-SPI ones). Would need a new dependency (e.g. `tesseract` bindings, which need
    a system library — breaks the "pure Rust, no system deps" property the rest of the
    vision pipeline currently has).

## How to test manually

```bash
cd /home/t14/Projects/omg-keys
cargo build --release
pkill -f "omg-keys daemon"; rm -f /run/user/1000/omg-keys.sock
RUST_LOG=info ./target/release/omg-keys daemon &   # or just wait for autostart next login
./target/release/omg-keys toggle    # opens/closes the grid (same as tapping R-Shift/Super)
./target/release/omg-keys hints     # opens/closes AT-SPI hint boxes
tail -f /tmp/omg-keysd.log          # if redirected there during manual runs
```

Take screenshots with `grim -o <output-name> /tmp/x.png` (`hyprctl -j monitors` for
output names) — full-resolution screenshots are large; downscale before viewing:
`magick /tmp/x.png -resize 1100x /tmp/x_small.png`.
