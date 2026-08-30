# OmaKeys

A keyboard-driven mouse replacement for Hyprland/wlroots (Wayland), written in Rust. Two
overlay modes let you point and click without ever touching the mouse:

- **Grid mode** — a fullscreen grid whose two-key cell codes mirror your physical keyboard
  layout onto screen position, so the mapping is spatial and learnable (`a`, home row/left
  pinky, sits middle-left of the screen; `p`, top row/right pinky, sits upper-right).
- **Hint mode** — scans the accessibility tree (AT-SPI) for buttons, text fields,
  scrollables, and terminals on screen and labels each one, Vimium-link-hint style. Type a
  label to warp the pointer there and click it.

## Requirements

- Hyprland on Wayland (developed/tested against Hyprland 0.56.2 via Omarchy)
- Rust (edition 2024)
- GTK4 + [`gtk4-layer-shell`](https://github.com/wmww/gtk4-layer-shell)
- [`wtype`](https://github.com/atx/wtype) on `PATH` — used to forward a keystroke to whatever
  you just clicked into (see "Type-to-commit" below)

## Build

```bash
cargo build --release
```

Produces `target/release/omakeys`.

## Setup

Run the daemon once per session — it owns the overlay windows and listens for commands over
a Unix socket:

```bash
./target/release/omakeys daemon &
```

Wire it into Hyprland's `~/.config/hypr/autostart.lua`:

```lua
o.launch_on_start("/path/to/omakeys/target/release/omakeys daemon")
```

And bind the toggles in `~/.config/hypr/bindings.lua`:

```lua
o.bind("SHIFT + SHIFT_R", "Toggle OmaKeys grid", "/path/to/omakeys/target/release/omakeys toggle", { release = true })
o.bind("SUPER + SUPER_L", "Toggle OmaKeys grid", "/path/to/omakeys/target/release/omakeys toggle", { release = true })
o.bind("CTRL + Control_R", "Toggle OmaKeys hints", "/path/to/omakeys/target/release/omakeys hints", { release = true })
```

The settings menu (see below) needs a genuine long-press, which Hyprland's own `long_press`
bind flag doesn't actually support (confirmed: it fires immediately on any press, not just
long ones). `scripts/super-hold-start.sh` / `scripts/super-hold-cancel.sh` implement real
hold-duration detection with a plain press/release timer pair instead:

```lua
o.bind("SUPER + SUPER_L", "Start OmaKeys menu hold-timer", "/path/to/omakeys/scripts/super-hold-start.sh")
o.bind("SUPER + SUPER_L", "Cancel OmaKeys menu hold-timer", "/path/to/omakeys/scripts/super-hold-cancel.sh", { release = true })
```

## Usage

| Key | Action |
|---|---|
| Right Shift / Super (tap) | Open grid mode |
| Right Ctrl | Open hint mode |
| Super (hold 3s+) | Open the settings/cheat-sheet menu |
| *(grid)* coarse key, then fine key | Pick a cell by its two-key code, warping the pointer there |
| *(grid)* `h`/`j`/`k`/`l` / arrows | Nudge the cursor (also usable immediately after a coarse pick, before the fine key) |
| Space | Click |
| Shift + Space | Right-click |
| Left Shift (hold) | Click-and-drag: press to start holding, nudge to select, release to finish |
| *(hints)* type a label | Warp to that target and click it |
| Backspace | Undo the last typed label character |
| Any other printable key while positioned | Click here, then forward that keystroke to whatever now has focus, so typing to start editing a field doesn't lose its first character |
| Escape | Close the overlay |

Once you've picked a cell or a hint target, the overlay gets out of the way entirely — no
dimming, no crosshair, just the real cursor — until you nudge, click, or close it.

### Cell-scoped auto-detection

The moment you pick a coarse cell in grid mode, OmaKeys kicks off a background AT-SPI scan
of just that region, looking for buttons. If it resolves before your next keystroke:

- **No buttons found** — nothing changes, the generic fine sub-grid stays as it is.
- **Exactly one** — the pointer warps there and clicks, no further keystroke needed at all.
- **More than one** — switches into labeled hint mode, scoped to just those buttons.

This never makes a keystroke slower (the fine grid always shows instantly regardless); it's
a pure opportunistic upgrade for whenever you pause even briefly after a coarse pick.

### Settings menu

Hold Super for 3+ seconds to open a small settings/cheat-sheet overlay. `+`/`-` adjust the
nudge step (how far each `h`/`j`/`k`/`l` press moves the cursor), saved to
`~/.config/omakeys/config.json`.

## How it works

- `omakeys daemon` owns a GTK4 + `gtk4-layer-shell` overlay and runs the main loop.
  `omakeys toggle` / `hints` / `menu` / `quit` are thin CLI clients that send one command
  over a Unix socket and exit — what Hyprland's keybinds actually invoke.
- Mouse movement goes through `wlr-virtual-pointer-unstable-v1` on its own raw
  `wayland-client` connection, bound to the focused output so coordinates stay in that
  output's own local space.
- Grid mode's coarse-pick view renders on 15 small layer-shell windows (one per coarse
  region) rather than one full-screen window, so Hyprland's `no_screen_share` layer-rule
  effect can — in principle — exclude the overlay from screen recordings while it stays
  visible on the physical display. In practice this only works when the overlay's own
  drawn footprint doesn't need to cover the whole screen; grid mode's coarse view
  necessarily does, so this is implemented but not (yet) usefully exclude-able — see
  `HANDOFF.md` for the full investigation.
- Hint mode merges two sources: AT-SPI accessibility elements (precise — real name,
  category, exact position, but only as good as each app's toolkit support) and a
  vision-based fallback (screenshot + classical edge/contour detection) that catches
  buttons/panels in apps AT-SPI structurally can't see into (Electron apps without
  accessibility force-enabled, GTK4 apps — see "Known limitations").

See [`HANDOFF.md`](HANDOFF.md) for the detailed build history, every bug found and fixed
along the way, and the full arc of approaches tried (and reverted) for vision-based
structural section detection.

## Known limitations

- **GTK4 apps report `(0, 0)` for every AT-SPI element's position on Wayland** — an upstream
  toolkit limitation, not fixable here. Hint mode drops these rather than drawing a garbage
  pile in the corner, so GTK4 apps currently only get vision-based hints, not precise
  AT-SPI ones.
- **Electron apps need accessibility force-enabled** to show up in AT-SPI at all (VS Code,
  Spotify, etc., unless launched with the right flag/env var) — vision-based hints cover
  these regardless.
- **Vision-based hints are coarse and unclassified** — "there's probably something clickable
  here," not "this is a save button." They're a fallback for what AT-SPI can't see, not a
  replacement for it.
- **Whole-structural-section hinting** (treat "the whole editor pane" or "the whole sidebar"
  as one clickable target) isn't implemented — extensively attempted via divider-line
  detection and ultimately abandoned as an unreliable signal (see `HANDOFF.md`). Grid mode
  is the fallback for these cases.

## Experimental: vision detectors

`src/vision.rs` has three independent, from-scratch box/line detectors beyond the one
actually wired into hint mode, each with its own animated-GIF visualizer for watching it run
against a real screenshot (dev tools, not part of normal operation):

```bash
omakeys visualize-growth --x 0 --y 0 --width 640 --height 480
omakeys visualize-fullscan --x 0 --y 0 --width 640 --height 480 [--check peak|uniformity]
omakeys visualize-fullscan-demo --demo band|uniform [--check peak|uniformity]
```

- **Seed growth** — scatter sparse seed points, ray-cast outward from each to the nearest
  edge, trace along it to find its real length, then keep only segments whose length is
  matched by another (a real box's opposite sides are the same length).
- **Full-scan voting** — exhaustively test every adjacent pixel pair for a color change,
  keep positions where enough rows/columns agree.
- Two optional filters for the full-scan detector, each catching a different false-positive
  pattern from ordinary text: **peak-ratio** (reject a position that's part of a broad band
  of similarly-dense neighbors) and **edge-magnitude uniformity** (reject a position whose
  edge step size varies a lot along its own length).

None of these are wired into the live hint-mode pipeline yet.

## Development

```bash
cargo test --release
```

35 tests, mostly synthetic-image-based (no live Wayland session needed) covering the grid
math and every vision detector's filter logic.
