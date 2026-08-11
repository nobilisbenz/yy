//! What the user was doing when they pressed the shortcut.
//!
//! Spec §18. Captured by the **daemon at toggle time, before the dock maps** — a moment
//! later the dock is itself the active window, and the answer would always be "brain-dock".
//!
//! Everything here is best-effort and every field is `Option`. Context *boosts* ranking, it
//! never filters (§4.2), so a property that is missing costs a little relevance and nothing
//! else. A failed read logs at debug and returns `None`; it must never fail a query.
//!
//! **The terminal problem.** For ghostty running nvim, `_NET_WM_PID` is the *terminal's*
//! pid and its `cwd` is wherever it was launched from — usually `$HOME`, which is useless.
//! The directory worth knowing belongs to the foreground descendant, so this walks
//! `/proc/<pid>/task/*/children` down to the deepest one. That walk is bounded by both depth
//! and a time budget because it runs on every summon.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};

use crate::{Atoms, X11Error};

/// How deep to follow the child chain. A terminal running a shell running an editor is
/// three; past about eight, something is wrong and the walk should stop rather than explore.
const MAX_DEPTH: usize = 8;

/// Total budget for the `/proc` descent. Spec §37 gives the whole capture 10 ms.
const DESCENT_BUDGET: Duration = Duration::from_millis(5);

/// The focused window, and what it is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub wm_class: Option<String>,
    pub window_title: Option<String>,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    /// The working directory of the deepest descendant, not of the terminal.
    pub cwd: Option<PathBuf>,
    pub workspace: Option<String>,
}

/// Read the active window's context.
///
/// Never returns an error for a missing property — only for a broken connection, and even
/// then the caller should carry on with an empty context.
pub fn capture(connection: &impl Connection, atoms: &Atoms, root: Window) -> Result<Context, X11Error> {
    let started = Instant::now();

    let Some(window) = active_window(connection, atoms, root)? else {
        // No focused window is normal: an empty workspace, or focus on the root.
        return Ok(Context::default());
    };

    let wm_class = wm_class(connection, window);
    let window_title = title(connection, atoms, window);
    let pid = pid(connection, atoms, window);

    let (process_name, cwd) = match pid {
        Some(pid) => {
            let deepest = deepest_descendant(pid);
            (read_comm(deepest), read_cwd(deepest))
        }
        None => (None, None),
    };

    let context = Context {
        wm_class,
        window_title,
        pid,
        process_name,
        cwd,
        workspace: workspace(connection, atoms, root),
    };

    tracing::debug!(
        elapsed_us = started.elapsed().as_micros() as u64,
        wm_class = ?context.wm_class,
        process = ?context.process_name,
        "captured desktop context"
    );
    Ok(context)
}

fn active_window(
    connection: &impl Connection,
    atoms: &Atoms,
    root: Window,
) -> Result<Option<Window>, X11Error> {
    let reply = connection
        .get_property(false, root, atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, 0, 1)?
        .reply()?;

    let window = reply.value32().and_then(|mut values| values.next());
    // Some window managers publish 0 to mean "none" rather than removing the property.
    Ok(window.filter(|id| *id != 0))
}

/// `WM_CLASS` is two NUL-separated strings: instance, then class. The class is the stable
/// one — the instance varies with how the program was launched.
fn wm_class(connection: &impl Connection, window: Window) -> Option<String> {
    let reply = connection
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;

    let mut parts = reply.value.split(|byte| *byte == 0);
    let instance = parts.next();
    let class = parts.next().filter(|part| !part.is_empty()).or(instance)?;
    Some(String::from_utf8_lossy(class).to_string())
}

/// `_NET_WM_NAME` (UTF-8) with a fall back to `WM_NAME` (latin-1), per spec §18.
fn title(connection: &impl Connection, atoms: &Atoms, window: Window) -> Option<String> {
    for (property, kind) in [
        (atoms._NET_WM_NAME, atoms.UTF8_STRING),
        (AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()),
    ] {
        let Ok(Ok(reply)) = connection
            .get_property(false, window, property, kind, 0, 1024)
            .map(|cookie| cookie.reply())
        else {
            continue;
        };
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).to_string());
        }
    }
    None
}

fn pid(connection: &impl Connection, atoms: &Atoms, window: Window) -> Option<u32> {
    connection
        .get_property(false, window, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
}

fn workspace(connection: &impl Connection, atoms: &Atoms, root: Window) -> Option<String> {
    let index = connection
        .get_property(false, root, atoms._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()?;
    Some(index.to_string())
}

/// Follow the process tree down to the deepest descendant.
///
/// This is what turns "ghostty, launched from `$HOME`" into "nvim, editing in
/// `~/projects/game`". Bounded by depth *and* by wall clock: it runs on the summon path, and
/// a pathological process tree must cost a bounded amount rather than however long it takes.
///
/// Takes the **first** child at each level. With multiple children there is no reliable way
/// to tell which one has the terminal's foreground, and guessing wrong is no worse than not
/// descending — both give a directory that only boosts ranking.
fn deepest_descendant(pid: u32) -> u32 {
    let deadline = Instant::now() + DESCENT_BUDGET;
    let mut current = pid;

    for _ in 0..MAX_DEPTH {
        if Instant::now() >= deadline {
            tracing::debug!(pid, "process descent hit its time budget");
            break;
        }
        match first_child(current) {
            Some(child) => current = child,
            None => break,
        }
    }
    current
}

/// The first child pid, read from `/proc/<pid>/task/*/children`.
fn first_child(pid: u32) -> Option<u32> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    for task in tasks.flatten() {
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        if let Some(first) = children.split_whitespace().next()
            && let Ok(child) = first.parse::<u32>()
        {
            return Some(child);
        }
    }
    None
}

fn read_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn read_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descent must terminate and stay cheap, whatever the process tree looks like.
    #[test]
    fn descending_from_our_own_process_is_bounded_and_fast() {
        let started = Instant::now();
        let deepest = deepest_descendant(std::process::id());
        let elapsed = started.elapsed();

        assert!(deepest > 0);
        // Spec §37 gives the whole capture 10 ms; this is the only unbounded-looking part.
        assert!(
            elapsed < Duration::from_millis(50),
            "the descent took {elapsed:?}"
        );
    }

    #[test]
    fn a_process_that_does_not_exist_yields_nothing_rather_than_failing() {
        // A window can outlive its process, and `_NET_WM_PID` can simply be wrong. Neither
        // is worth failing a query over.
        let gone = u32::MAX;
        assert_eq!(deepest_descendant(gone), gone);
        assert_eq!(read_comm(gone), None);
        assert_eq!(read_cwd(gone), None);
        assert_eq!(first_child(gone), None);
    }

    #[test]
    fn our_own_process_reports_its_name_and_directory() {
        let me = std::process::id();
        assert!(read_comm(me).is_some());
        assert_eq!(read_cwd(me), std::env::current_dir().ok());
    }
}
