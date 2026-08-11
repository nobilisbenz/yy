//! Where the dock goes.
//!
//! Anchored to the *work area*, not the monitor rectangle. polybar owns the top
//! 30 px of this display, and `y = monitor.y + margin` slides the dock
//! underneath it.
//!
//! The obvious source for that is `_NET_WORKAREA`, but **i3 does not publish
//! it** — verified on this machine, where `xprop -root _NET_WORKAREA` reports
//! "not found". So the usable area is derived from panel struts directly, which
//! is where a window manager gets it from anyway. polybar advertises
//! `_NET_WM_STRUT_PARTIAL = 0, 0, 30, 0`, and honouring that puts the dock
//! below the bar on any WM, with bar toggling handled for free.

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};

use crate::X11Error;
use crate::atoms::Atoms;

/// A rectangle in root-window coordinates, physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }

    fn center(&self) -> (i32, i32) {
        (
            self.x + self.width as i32 / 2,
            self.y + self.height as i32 / 2,
        )
    }

    /// Intersection, or `None` if they do not overlap.
    fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width as i32).min(other.x + other.width as i32);
        let bottom = (self.y + self.height as i32).min(other.y + other.height as i32);
        (right > x && bottom > y).then(|| Rect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    TopRight,
    TopLeft,
    TopCenter,
}

#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub anchor: Anchor,
    pub margin_top: i32,
    pub margin_side: i32,
    pub width: u32,
    pub height: u32,
}

impl Placement {
    /// Top-left corner for this placement within `area`.
    pub fn position_in(&self, area: &Rect) -> (i32, i32) {
        let x = match self.anchor {
            Anchor::TopRight => area.x + area.width as i32 - self.width as i32 - self.margin_side,
            Anchor::TopLeft => area.x + self.margin_side,
            Anchor::TopCenter => area.x + (area.width as i32 - self.width as i32) / 2,
        };
        (x, area.y + self.margin_top)
    }
}

/// Pick the monitor the user is looking at, then narrow it to the usable area.
///
/// Order is spec §7: focused window, then pointer, then primary, then whatever
/// exists. Each step is best-effort; a failure falls through rather than
/// aborting the summon.
pub fn active_work_area(
    conn: &impl Connection,
    root: Window,
    atoms: &Atoms,
) -> Result<Rect, X11Error> {
    let monitors = monitors(conn, root)?;
    let monitor = monitor_for_focus(conn, root, atoms, &monitors)
        .or_else(|| monitor_for_pointer(conn, root, &monitors))
        .or_else(|| monitors.first().copied())
        .ok_or(X11Error::NoMonitors)?;

    // `_NET_WORKAREA` is a single rectangle spanning the whole desktop, not one
    // per monitor, so intersect it with the chosen monitor.
    //
    // i3 does not publish `_NET_WORKAREA` at all, so the strut scan below is
    // the path that actually runs here — but prefer the property when a WM
    // offers it, since it is one round trip instead of one per window.
    let desktop = match workarea(conn, root, atoms)? {
        Some(area) => area,
        None => strut_work_area(conn, root, atoms)?,
    };

    Ok(desktop.intersect(&monitor).unwrap_or(monitor))
}

/// Derive the usable area from panel struts.
///
/// `_NET_WM_STRUT_PARTIAL` is how a panel reserves screen edge space, and it is
/// what the window manager itself reads. polybar publishes `0,0,30,0` here;
/// without honouring it the dock lands underneath the bar.
///
/// Docks are frequently absent from `_NET_CLIENT_LIST`, so walk the root's
/// children rather than the client list.
fn strut_work_area(
    conn: &impl Connection,
    root: Window,
    atoms: &Atoms,
) -> Result<Rect, X11Error> {
    let screen = {
        let geometry = conn.get_geometry(root)?.reply()?;
        Rect {
            x: 0,
            y: 0,
            width: geometry.width as u32,
            height: geometry.height as u32,
        }
    };

    // Walk a few levels, not just the root's children. i3 reparents managed
    // windows — including docks — into frame windows, so polybar is a
    // grandchild of the root and a one-level scan finds only i3's own bar.
    // Three levels covers i3's framing with room to spare; the tree is a few
    // dozen windows, so this stays cheap.
    let windows = descendants(conn, root, 3)?;

    // Pipeline every request before blocking on the first reply; otherwise this
    // is one full round trip per window on the summon path.
    let mut pending = Vec::with_capacity(windows.len() * 2);
    for window in windows {
        pending.push((
            conn.get_property(false, window, atoms._NET_WM_STRUT_PARTIAL, u32::from(AtomEnum::CARDINAL), 0, 12)?,
            conn.get_property(false, window, atoms._NET_WM_STRUT, u32::from(AtomEnum::CARDINAL), 0, 4)?,
        ));
    }

    let (mut left, mut right, mut top, mut bottom) = (0u32, 0u32, 0u32, 0u32);
    for (partial, plain) in pending {
        // `_NET_WM_STRUT_PARTIAL` supersedes `_NET_WM_STRUT` when both exist.
        let values: Option<Vec<u32>> = partial
            .reply()
            .ok()
            .and_then(|reply| reply.value32().map(|v| v.collect()))
            .filter(|v: &Vec<u32>| v.len() >= 4)
            .or_else(|| {
                plain
                    .reply()
                    .ok()
                    .and_then(|reply| reply.value32().map(|v| v.collect()))
                    .filter(|v: &Vec<u32>| v.len() >= 4)
            });

        if let Some(v) = values {
            left = left.max(v[0]);
            right = right.max(v[1]);
            top = top.max(v[2]);
            bottom = bottom.max(v[3]);
        }
    }

    if left | right | top | bottom == 0 {
        tracing::debug!("no panel struts found; using the full screen");
        return Ok(screen);
    }
    tracing::debug!(left, right, top, bottom, "work area from panel struts");

    Ok(Rect {
        x: screen.x + left as i32,
        y: screen.y + top as i32,
        width: screen.width.saturating_sub(left + right),
        height: screen.height.saturating_sub(top + bottom),
    })
}

/// Every window within `depth` levels of `root`, breadth first.
fn descendants(
    conn: &impl Connection,
    root: Window,
    depth: u32,
) -> Result<Vec<Window>, X11Error> {
    let mut found = Vec::new();
    let mut frontier = vec![root];

    for _ in 0..depth {
        if frontier.is_empty() {
            break;
        }
        // One batch of requests per level rather than one per window.
        let pending: Vec<_> = frontier
            .iter()
            .map(|&w| conn.query_tree(w))
            .collect::<Result<_, _>>()?;

        frontier = pending
            .into_iter()
            .filter_map(|cookie| cookie.reply().ok())
            .flat_map(|reply| reply.children)
            .collect();
        found.extend_from_slice(&frontier);
    }

    Ok(found)
}

/// Physical monitor rectangles via RandR.
pub fn monitors(conn: &impl Connection, root: Window) -> Result<Vec<Rect>, X11Error> {
    let reply = conn.randr_get_monitors(root, true)?.reply()?;

    // `get_monitors` lists the primary first, which makes it the natural
    // last-resort fallback without a separate query.
    let mut rects: Vec<Rect> = reply
        .monitors
        .iter()
        .map(|m| Rect {
            x: m.x as i32,
            y: m.y as i32,
            width: m.width as u32,
            height: m.height as u32,
        })
        .collect();

    if let Some(primary) = reply.monitors.iter().position(|m| m.primary) {
        rects.swap(0, primary);
    }
    Ok(rects)
}

fn monitor_for_focus(
    conn: &impl Connection,
    root: Window,
    atoms: &Atoms,
    monitors: &[Rect],
) -> Option<Rect> {
    let active = active_window(conn, root, atoms).ok().flatten()?;
    let geometry = conn.get_geometry(active).ok()?.reply().ok()?;
    // The geometry is relative to the window's parent, which under a reparenting
    // WM is a frame, not the root. Translate before comparing.
    let absolute = conn
        .translate_coordinates(active, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;

    let rect = Rect {
        x: absolute.dst_x as i32,
        y: absolute.dst_y as i32,
        width: geometry.width as u32,
        height: geometry.height as u32,
    };
    let (cx, cy) = rect.center();
    monitors.iter().find(|m| m.contains(cx, cy)).copied()
}

fn monitor_for_pointer(conn: &impl Connection, root: Window, monitors: &[Rect]) -> Option<Rect> {
    let pointer = conn.query_pointer(root).ok()?.reply().ok()?;
    monitors
        .iter()
        .find(|m| m.contains(pointer.root_x as i32, pointer.root_y as i32))
        .copied()
}

/// Currently focused window, per EWMH.
pub fn active_window(
    conn: &impl Connection,
    root: Window,
    atoms: &Atoms,
) -> Result<Option<Window>, X11Error> {
    let reply = conn
        .get_property(false, root, atoms._NET_ACTIVE_WINDOW, u32::from(AtomEnum::WINDOW), 0, 1)?
        .reply()?;

    Ok(reply
        .value32()
        .and_then(|mut v| v.next())
        .filter(|&w| w != x11rb::NONE))
}

fn workarea(conn: &impl Connection, root: Window, atoms: &Atoms) -> Result<Option<Rect>, X11Error> {
    let reply = conn
        .get_property(false, root, atoms._NET_WORKAREA, u32::from(AtomEnum::CARDINAL), 0, 4)?
        .reply()?;

    let Some(mut values) = reply.value32() else {
        return Ok(None);
    };
    let (Some(x), Some(y), Some(width), Some(height)) = (
        values.next(),
        values.next(),
        values.next(),
        values.next(),
    ) else {
        return Ok(None);
    };

    Ok(Some(Rect {
        x: x as i32,
        y: y as i32,
        width,
        height,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    // What this machine actually reports with polybar running: 30px off the top.
    const WORK_AREA: Rect = Rect { x: 0, y: 30, width: 1920, height: 1050 };

    fn placement() -> Placement {
        Placement {
            anchor: Anchor::TopRight,
            margin_top: 8,
            margin_side: 22,
            width: 560,
            height: 62,
        }
    }

    #[test]
    fn top_right_sits_inside_the_work_area() {
        let (x, y) = placement().position_in(&WORK_AREA);
        assert_eq!(x, 1920 - 560 - 22);
        // Below the bar, not under it. This is the bug the work area prevents.
        assert_eq!(y, 38);
        assert!(y >= WORK_AREA.y);
    }

    #[test]
    fn a_second_monitor_gets_its_own_offsets() {
        let right_monitor = Rect { x: 1920, y: 0, width: 2560, height: 1440 };
        let (x, y) = placement().position_in(&right_monitor);
        assert_eq!(x, 1920 + 2560 - 560 - 22);
        assert_eq!(y, 8);
    }

    #[test]
    fn work_area_clips_to_the_chosen_monitor() {
        let desktop = Rect { x: 0, y: 30, width: 4480, height: 1410 };
        let second = Rect { x: 1920, y: 0, width: 2560, height: 1440 };
        let clipped = desktop.intersect(&second).unwrap();
        assert_eq!(clipped.x, 1920);
        assert_eq!(clipped.y, 30);
        assert_eq!(clipped.width, 2560);
    }

    #[test]
    fn disjoint_rectangles_do_not_intersect() {
        let a = Rect { x: 0, y: 0, width: 100, height: 100 };
        let b = Rect { x: 200, y: 200, width: 100, height: 100 };
        assert!(a.intersect(&b).is_none());
    }

    #[test]
    fn contains_excludes_the_far_edge() {
        assert!(FULL_HD.contains(0, 0));
        assert!(FULL_HD.contains(1919, 1079));
        assert!(!FULL_HD.contains(1920, 540));
    }

    #[test]
    fn other_anchors_place_sensibly() {
        let mut p = placement();
        p.anchor = Anchor::TopLeft;
        assert_eq!(p.position_in(&WORK_AREA).0, 22);

        p.anchor = Anchor::TopCenter;
        assert_eq!(p.position_in(&WORK_AREA).0, (1920 - 560) / 2);
    }
}
