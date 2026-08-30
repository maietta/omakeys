//! Scans the AT-SPI accessibility tree for interactive elements (buttons, text fields,
//! scrollable views, terminals, ...) on screen, so the overlay can draw a "hint" box over
//! each one.
//!
//! Only applications that expose an accessibility tree (GTK, Qt, Electron with a11y
//! enabled, Firefox, LibreOffice, ...) will show up here.
//!
//! Two Wayland-specific limitations shape this module:
//!
//! - AT-SPI has no global desktop coordinate space under Wayland (clients can't know their
//!   own absolute position), so element extents come back relative to the app's own window,
//!   not the compositor or even the monitor. We match each AT-SPI application frame to a
//!   [`FocusedWindow`] by title (populated from Hyprland, which *does* know real window
//!   positions) and add that window's monitor-local offset to every element found inside it,
//!   turning window-local coordinates into coordinates the overlay can draw directly.
//! - GTK4 apps additionally don't implement `Component` position tracking on Wayland at all
//!   (width/height come through, but x/y are always reported as exactly `(0, 0)`). There's
//!   no way to distinguish that from a real element genuinely at the origin, so such
//!   elements are dropped rather than drawn as a garbage pile in the corner.

use std::time::Duration;

use anyhow::Result;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::{AccessibilityConnection, CoordType, Role, State};
use futures_lite::FutureExt;
use zbus::names::OwnedUniqueName;
use zbus::Connection;
use zvariant::OwnedObjectPath;

use crate::active_monitor::FocusedWindow;

/// The AT-SPI registry accumulates stale entries for applications that crashed or
/// disconnected without deregistering; calls to those hang instead of erroring (there's no
/// peer left to reply). Bound how long we'll wait on any one application so a single stale
/// entry can't wedge the whole scan.
const APP_SCAN_TIMEOUT: Duration = Duration::from_millis(1500);

/// Race `fut` against a timeout, returning `None` if the timeout wins.
async fn with_timeout<T>(fut: impl std::future::Future<Output = T>, dur: Duration) -> Option<T> {
    async { Some(fut.await) }
        .or(async {
            glib::timeout_future(dur).await;
            None
        })
        .await
}

/// Broad category, used to color-code hints in the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Button,
    TextField,
    Scrollable,
    Terminal,
    Other,
}

/// An interactive element found in a window on the focused monitor, in pixel coordinates
/// local to that monitor (see the module docs for why).
#[derive(Debug, Clone)]
pub struct Element {
    pub role: Role,
    pub category: Category,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Element {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Classify a role into a hint category, or `None` if it's not worth hinting.
fn categorize(role: Role) -> Option<Category> {
    match role {
        Role::PushButton
        | Role::ToggleButton
        | Role::RadioButton
        | Role::RadioMenuItem
        | Role::CheckBox
        | Role::CheckMenuItem
        | Role::ComboBox
        | Role::Link
        | Role::MenuItem
        | Role::TearoffMenuItem
        | Role::PushButtonMenu
        | Role::Slider
        | Role::SpinButton => Some(Category::Button),

        Role::Entry | Role::PasswordText => Some(Category::TextField),

        Role::ScrollPane | Role::Viewport | Role::DocumentFrame | Role::ScrollBar => {
            Some(Category::Scrollable)
        }

        Role::Terminal => Some(Category::Terminal),

        _ => None,
    }
}

/// Connect to the AT-SPI bus, find every application frame whose name matches one of
/// `windows` (every window on the focused monitor, per Hyprland), and walk each matching
/// frame's accessibility tree, adding that window's monitor-local offset to every element
/// found inside it. Best-effort: individual apps or objects that error out (stale
/// references, unsupported interfaces, etc.) are skipped.
pub async fn scan_interactive_elements(windows: &[FocusedWindow]) -> Result<Vec<Element>> {
    let a11y = AccessibilityConnection::new().await?;
    let conn = a11y.connection().clone();

    let mut elements = Vec::new();

    let root = AccessibleProxy::builder(&conn)
        .destination("org.a11y.atspi.Registry")?
        .path("/org/a11y/atspi/accessible/root")?
        .build()
        .await?;

    let apps = root.get_children().await.unwrap_or_default();

    for app in apps {
        let dest = app.name.clone();
        let timed_out = with_timeout(
            scan_app(&conn, &app.name, &app.path, windows, &mut elements),
            APP_SCAN_TIMEOUT,
        )
        .await
        .is_none();
        if timed_out {
            log::warn!("omg-keys: AT-SPI app {dest} timed out during scan, skipping");
        }
    }

    Ok(elements)
}

/// Look for frames among `app`'s children whose name matches one of `windows` and walk each
/// one found, offsetting its elements by that window's monitor-local position.
async fn scan_app(
    conn: &Connection,
    app_dest: &OwnedUniqueName,
    app_path: &OwnedObjectPath,
    windows: &[FocusedWindow],
    out: &mut Vec<Element>,
) {
    let Ok(app_accessible) = AccessibleProxy::builder(conn)
        .destination(app_dest.clone())
        .and_then(|b| b.path(app_path.clone()))
    else {
        return;
    };
    let Ok(app_accessible) = app_accessible.build().await else {
        return;
    };
    let Ok(frames) = app_accessible.get_children().await else {
        return;
    };

    for frame in frames {
        let Ok(frame_accessible) = AccessibleProxy::builder(conn)
            .destination(frame.name.clone())
            .and_then(|b| b.path(frame.path.clone()))
        else {
            continue;
        };
        let Ok(frame_accessible) = frame_accessible.build().await else {
            continue;
        };
        let frame_title = frame_accessible.name().await.unwrap_or_default();
        let Some(window) = windows.iter().find(|w| w.title == frame_title) else {
            continue;
        };
        walk(conn, &frame.name, &frame.path, out, 0, window.x, window.y).await;
    }
}

const MAX_DEPTH: usize = 40;

async fn walk(
    conn: &Connection,
    dest: &OwnedUniqueName,
    path: &OwnedObjectPath,
    out: &mut Vec<Element>,
    depth: usize,
    offset_x: f64,
    offset_y: f64,
) {
    if depth > MAX_DEPTH {
        return;
    }

    let Ok(accessible) = AccessibleProxy::builder(conn)
        .destination(dest.clone())
        .and_then(|b| b.path(path.clone()))
    else {
        return;
    };
    let Ok(accessible) = accessible.build().await else {
        return;
    };

    let Ok(state) = accessible.get_state().await else {
        return;
    };
    // Application/Frame-level objects often carry no Showing/Visible state at all, so only
    // use it to decide whether *this* node is worth hinting, not whether to keep recursing.
    let showing = state.contains(State::Showing);

    if let Ok(role) = accessible.get_role().await {
        if showing {
            if let Some(category) = categorize(role) {
            if let Ok(component) = ComponentProxy::builder(conn)
                .destination(dest.clone())
                .and_then(|b| b.path(path.clone()))
            {
                if let Ok(component) = component.build().await {
                    if let Ok((x, y, w, h)) = component.get_extents(CoordType::Screen).await {
                        // (0, 0) is what GTK4 apps report for every element on Wayland
                        // (position tracking isn't implemented there) -- indistinguishable
                        // from a real element at the origin, so treat it as "unknown" and
                        // skip rather than draw a garbage pile in the corner.
                        if w > 0 && h > 0 && (x, y) != (0, 0) {
                            let name = accessible.name().await.unwrap_or_default();
                            out.push(Element {
                                role,
                                category,
                                name,
                                x: x as f64 + offset_x,
                                y: y as f64 + offset_y,
                                w: w as f64,
                                h: h as f64,
                            });
                        }
                    }
                }
            }
        }
        }
    }

    let Ok(children) = accessible.get_children().await else {
        return;
    };

    for child in children {
        Box::pin(walk(conn, &child.name, &child.path, out, depth + 1, offset_x, offset_y)).await;
    }
}
