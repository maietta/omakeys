#!/usr/bin/env bash
# Bound to Super's key-*release* in ~/.config/hypr/bindings.lua, alongside the existing
# (unconditional, unchanged) release-triggered grid-toggle bind. Cancels the timer started by
# super-hold-start.sh if Super is released before it fires -- i.e. an ordinary tap. If the
# hold was already 3+ seconds, the timer has already fired (opened the menu) and this is a
# harmless no-op; the existing grid-toggle bind still fires too, since it's unconditional.
# See super-hold-start.sh for why this exists instead of Hyprland's own `long_press` flag.

set -euo pipefail

PIDFILE="${XDG_RUNTIME_DIR:-/tmp}/omg-keys-super-hold.pid"

if [ -f "$PIDFILE" ]; then
    kill "$(cat "$PIDFILE")" 2>/dev/null || true
    rm -f "$PIDFILE"
fi
