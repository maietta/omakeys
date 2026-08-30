#!/usr/bin/env bash
# Bound to Super's key-*press* in ~/.config/hypr/bindings.lua. Starts a background timer that
# opens the omakeys settings menu after a genuine 3-second hold. Paired with
# super-hold-cancel.sh (bound to Super's key-*release*), which kills this timer if Super is
# let go before it fires -- i.e. an ordinary tap.
#
# This exists because Hyprland's own `long_press` bind flag does not discriminate hold
# duration at all (confirmed live: it fires immediately on every press, same as a normal
# tap) -- see omakeys/HANDOFF.md's "Gotchas" section. Implementing the timing ourselves
# with a plain press/release script pair sidesteps that entirely.

set -euo pipefail

PIDFILE="${XDG_RUNTIME_DIR:-/tmp}/omakeys-super-hold.pid"
BIN="$(dirname "$(readlink -f "$0")")/../target/release/omakeys"

(sleep 3 && "$BIN" menu) &
echo $! > "$PIDFILE"
