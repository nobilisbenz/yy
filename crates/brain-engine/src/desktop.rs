//! The `.desktop` application index.
//!
//! Spec §32. `@app blender` in a note is hand-written and will not always be the exact
//! desktop id — it may be the name you call the program, or a prefix of a reverse-DNS id
//! nobody types in full. So matching is deliberately forgiving, in a fixed order: exact id,
//! then exact name, then a unique prefix of an id or name.
//!
//! **Launching goes through `gtk-launch <id>`**, not through the `Exec` line. `Exec` carries
//! field codes (`%f`, `%U`, `%i`, `%c`, `%k`) that have to be expanded or stripped according
//! to rules that vary by code, and getting them wrong breaks specific applications in ways
//! that only show up for those applications. `gtk-launch` also does startup notification.
//! Hand-parsing `Exec` is a well-known way to be subtly wrong forever.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One installed application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApp {
    /// Filename minus `.desktop` — what `gtk-launch` takes.
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// Everything installed, indexed by id.
#[derive(Debug, Default, Clone)]
pub struct DesktopIndex {
    apps: BTreeMap<String, DesktopApp>,
}

impl DesktopIndex {
    /// Scan the standard locations.
    ///
    /// Never fails: a missing or unreadable directory means fewer applications, not a
    /// broken daemon. `@app` is one action kind among several.
    pub fn scan() -> Self {
        let mut index = Self::default();
        for directory in search_paths() {
            index.scan_directory(&directory);
        }
        tracing::debug!(applications = index.apps.len(), "desktop index built");
        index
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    fn scan_directory(&mut self, directory: &Path) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(app) = parse_desktop_entry(&text, &path) else {
                continue;
            };
            // Earlier directories win: `~/.local/share/applications` is meant to shadow
            // the system copy of the same id, which is how a user overrides one.
            self.apps.entry(app.id.clone()).or_insert(app);
        }
    }

    /// Resolve what a note wrote to an installed application.
    pub fn resolve(&self, wanted: &str) -> Option<&DesktopApp> {
        let wanted = wanted.trim().to_lowercase();
        if wanted.is_empty() {
            return None;
        }

        if let Some(exact) = self
            .apps
            .values()
            .find(|app| app.id.to_lowercase() == wanted)
        {
            return Some(exact);
        }
        if let Some(named) = self
            .apps
            .values()
            .find(|app| app.name.to_lowercase() == wanted)
        {
            return Some(named);
        }

        // Prefix, but only when it is unambiguous: `@app g` should not silently launch
        // whichever of gimp, gedit, and google-chrome sorted first.
        let mut candidates = self.apps.values().filter(|app| {
            let id = app.id.to_lowercase();
            id.starts_with(&wanted)
                || id.rsplit('.').next().is_some_and(|tail| tail == wanted)
                || app.name.to_lowercase().starts_with(&wanted)
        });
        let first = candidates.next()?;
        candidates.next().is_none().then_some(first)
    }
}

/// `$XDG_DATA_HOME/applications`, then each `$XDG_DATA_DIRS/applications`.
fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        paths.push(PathBuf::from(home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".local/share/applications"));
    }

    let dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for directory in dirs.split(':').filter(|part| !part.is_empty()) {
        paths.push(PathBuf::from(directory).join("applications"));
    }

    paths
}

/// Parse the `[Desktop Entry]` group.
///
/// Returns `None` for entries that should not be offered: `NoDisplay` and `Hidden` mark
/// helpers and tombstones that exist to satisfy MIME associations, not things a user
/// launches.
fn parse_desktop_entry(text: &str, path: &Path) -> Option<DesktopApp> {
    let mut in_entry = false;
    let mut name = None;
    let mut hidden = false;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // Only the first group is the entry itself; the rest are actions.
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            // Plain `Name`, not `Name[de]` — the localised variants would otherwise
            // overwrite it depending on file order.
            "Name" if name.is_none() => name = Some(value.trim().to_string()),
            "NoDisplay" | "Hidden" if value.trim() == "true" => hidden = true,
            _ => {}
        }
    }

    if hidden {
        return None;
    }

    let id = path.file_stem()?.to_string_lossy().to_string();
    let name = name.unwrap_or_else(|| id.clone());
    Some(DesktopApp {
        id,
        name,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(entries: &[(&str, &str)]) -> DesktopIndex {
        let mut apps = BTreeMap::new();
        for (id, name) in entries {
            apps.insert(
                (*id).to_string(),
                DesktopApp {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    path: PathBuf::from(format!("/usr/share/applications/{id}.desktop")),
                },
            );
        }
        DesktopIndex { apps }
    }

    #[test]
    fn an_exact_id_wins_over_everything() {
        let apps = index(&[("blender", "Blender"), ("blender-nightly", "Blender Nightly")]);
        assert_eq!(apps.resolve("blender").unwrap().id, "blender");
    }

    #[test]
    fn a_note_can_use_the_name_it_calls_the_program() {
        // `@app OBS Studio` is what a person writes; the id is reverse-DNS.
        let apps = index(&[("com.obsproject.Studio", "OBS Studio")]);
        assert_eq!(
            apps.resolve("OBS Studio").unwrap().id,
            "com.obsproject.Studio"
        );
        assert_eq!(apps.resolve("obs studio").unwrap().id, "com.obsproject.Studio");
    }

    #[test]
    fn the_last_segment_of_a_reverse_dns_id_resolves() {
        // Nobody writes `@app com.obsproject.Studio` from memory.
        let apps = index(&[("com.obsproject.Studio", "OBS Studio")]);
        assert_eq!(apps.resolve("studio").unwrap().id, "com.obsproject.Studio");
    }

    #[test]
    fn an_ambiguous_prefix_resolves_to_nothing_rather_than_a_guess() {
        // Launching whichever of these sorted first would be a coin flip the user cannot
        // see, and the failure would look like the note being wrong.
        let apps = index(&[("gimp", "GIMP"), ("gedit", "Text Editor")]);
        assert!(apps.resolve("g").is_none());
        // Unambiguous prefixes still work.
        assert_eq!(apps.resolve("gim").unwrap().id, "gimp");
    }

    #[test]
    fn an_unknown_app_resolves_to_nothing() {
        let apps = index(&[("blender", "Blender")]);
        assert!(apps.resolve("definitely-not-installed").is_none());
        assert!(apps.resolve("").is_none());
    }

    #[test]
    fn hidden_and_nodisplay_entries_are_not_offered() {
        let path = PathBuf::from("/usr/share/applications/helper.desktop");
        assert!(
            parse_desktop_entry("[Desktop Entry]\nName=Helper\nNoDisplay=true\n", &path).is_none()
        );
        assert!(parse_desktop_entry("[Desktop Entry]\nName=Old\nHidden=true\n", &path).is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nName=Real\n", &path).is_some());
    }

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        // A `[Desktop Action new-window]` group has its own Name, and taking it would
        // label the button with the action rather than the application.
        let path = PathBuf::from("/usr/share/applications/term.desktop");
        let app = parse_desktop_entry(
            "[Desktop Entry]\nName=Ghostty\n\n[Desktop Action new-window]\nName=New Window\n",
            &path,
        )
        .unwrap();
        assert_eq!(app.name, "Ghostty");
        assert_eq!(app.id, "term");
    }

    /// The real system, if there is one. Skips rather than fails in a bare container.
    #[test]
    fn scanning_the_real_system_finds_applications() {
        let apps = DesktopIndex::scan();
        if apps.is_empty() {
            eprintln!("no .desktop files on this machine; skipping");
            return;
        }
        assert!(apps.len() > 1);
    }
}
