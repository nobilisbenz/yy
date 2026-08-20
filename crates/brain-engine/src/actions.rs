//! Turning a retrieved section into buttons, and running them.
//!
//! Stage 1 §1.8. Three rules, and each of them is load-bearing:
//!
//! **Never a shell.** Templates expand into individual `argv` elements and go straight to
//! `execvp`. A path containing a space, a quote, a `$`, or a newline has to survive
//! unchanged, and the only way to guarantee that is for no shell to ever see it.
//!
//! **Never model output.** An action is built from parsed vault metadata — a section's
//! path and line, an `@action` line the parser read — and never from anything a model
//! produced. This is spec §48, and the enum enforces it structurally: there is no
//! `RunShell` variant to reach for, so the unsafe thing is not merely discouraged, it is
//! unspellable.
//!
//! **Detached.** The editor you open must outlive the daemon that opened it. Without a new
//! process group, a `systemctl restart brain-daemon` takes your open nvim with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use brain_core::config::Openers;
use brain_core::{ActionId, config};
use brain_proto::{ActionKind, ActionView};
use yalive::model::ActionRow;

use crate::Ranked;
use crate::desktop::DesktopIndex;

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("no action with id {0}")]
    Unknown(ActionId),
    #[error("{program} is not installed")]
    NotInstalled { program: String },
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// What an action actually does, resolved from trusted data.
///
/// Deliberately a closed set. Adding "run this string" here would be the single change that
/// undoes the guarantee the rest of the module exists to provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Open a file, optionally at a line.
    File { path: PathBuf, line: usize },
    Url { url: String },
    /// A URL with a timestamp, for a video note.
    Video { url: String, seconds: Option<u64> },
    Directory { path: PathBuf },
    /// A `.desktop` id, launched through `gtk-launch`.
    App { id: String, name: String },
}

/// The order buttons appear in, and therefore what `Alt+1` means.
///
/// Stable ordering matters more than optimal ordering: `Alt+1` becomes muscle memory, and a
/// button that moves depending on what a query happened to retrieve is worse than one in a
/// slightly suboptimal place. Note is always first because it is the one pressed most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Note = 0,
    Code = 1,
    Video = 2,
    App = 3,
    Project = 4,
    Url = 5,
}

/// One offered action: what the button says, and what it will do.
#[derive(Debug, Clone)]
pub struct Action {
    pub id: ActionId,
    pub kind: ActionKind,
    pub label: String,
    pub target: Target,
    /// A target that failed validation — a path that no longer exists. Rendered disabled
    /// rather than hidden: a broken link you can see beats one that silently vanished.
    pub enabled: bool,
}

impl Action {
    pub fn view(&self) -> ActionView {
        ActionView {
            id: self.id,
            kind: self.kind,
            label: self.label.clone(),
            detail: self.detail(),
            enabled: self.enabled,
        }
    }

    fn detail(&self) -> String {
        match &self.target {
            Target::File { path, line } => format!("{}:{line}", display_path(path)),
            Target::Url { url } => url.clone(),
            Target::Video { url, seconds } => match seconds {
                Some(seconds) => format!("{url} @{}", format_timestamp(*seconds)),
                None => url.clone(),
            },
            Target::Directory { path } => display_path(path),
            Target::App { id, .. } => id.clone(),
        }
    }
}

/// Abbreviate `$HOME` back to `~` for display only. Never for the argv.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = home.to_string_lossy().to_string();
            match text.strip_prefix(&home) {
                Some(rest) => format!("~{rest}"),
                None => text,
            }
        }
        None => text,
    }
}

/// `Alt+1` … `Alt+9`. A tenth button has no keystroke, so there is no tenth button.
const MAX_ACTIONS: usize = 9;

/// Build the actions offered for a ranked result set.
///
/// Two sources feed this: an implicit `Open note` per retrieved section — the `[Note]`
/// button, and the one pressed most — plus whatever `@action` lines the author declared on
/// those sections, resolved here from trusted parsed metadata.
///
/// Results are deduplicated by target, so three sections referencing the same video produce
/// one button, and ordered by [`Slot`] so position is stable across queries.
pub fn for_results(results: &[Ranked], declared: &[ActionRow], apps: &DesktopIndex) -> Vec<Action> {
    // Rank of the section that declared each action, so a `@video` on the top result
    // outranks one on the fifth.
    let rank_of: HashMap<&str, usize> = results
        .iter()
        .enumerate()
        .map(|(rank, entry)| (entry.hit.section_uid.as_str(), rank))
        .collect();

    let mut candidates: Vec<(Slot, usize, String, Target, bool)> = Vec::new();

    // The implicit per-section note button.
    for (rank, entry) in results.iter().enumerate() {
        let path = entry.hit.path.clone();
        let label = if entry.hit.heading.is_empty() {
            entry.hit.note_title.clone()
        } else {
            entry.hit.heading.clone()
        };
        let exists = path.exists();
        candidates.push((
            Slot::Note,
            rank,
            label,
            Target::File {
                path,
                line: entry.hit.start_line.max(1),
            },
            exists,
        ));
    }

    for row in declared {
        let rank = rank_of.get(row.section_uid.as_str()).copied().unwrap_or(usize::MAX);
        let Some((slot, label, target, enabled)) = resolve(row, apps) else {
            continue;
        };
        candidates.push((slot, rank, label, target, enabled));
    }

    // Stable sort by slot then rank: within a slot, the better-ranked section's action
    // comes first, and the slot ordering never depends on the query.
    candidates.sort_by_key(|(slot, rank, _, _, _)| (*slot, *rank));

    let mut actions: Vec<Action> = Vec::new();
    let mut seen: Vec<Target> = Vec::new();

    for (slot, _, label, target, enabled) in candidates {
        // Three sections referencing the same file or video are one button.
        if seen.contains(&target) {
            continue;
        }
        seen.push(target.clone());

        actions.push(Action {
            id: ActionId(actions.len() as i64 + 1),
            kind: kind_for(slot),
            label,
            target,
            enabled,
        });

        if actions.len() >= MAX_ACTIONS {
            break;
        }
    }

    actions
}

fn kind_for(slot: Slot) -> ActionKind {
    match slot {
        Slot::Note | Slot::Code => ActionKind::OpenFile,
        Slot::Video => ActionKind::OpenVideo,
        Slot::App => ActionKind::LaunchDesktopApp,
        Slot::Project => ActionKind::OpenProject,
        Slot::Url => ActionKind::OpenUrl,
    }
}

/// Turn one stored `@action` row into a button.
///
/// Validation happens here rather than being skipped: a `@file` pointing at a path that no
/// longer exists becomes a **disabled** button, not a missing one. A dead link you can see
/// tells you the note needs fixing; one that silently vanished tells you nothing.
fn resolve(row: &ActionRow, apps: &DesktopIndex) -> Option<(Slot, String, Target, bool)> {
    match row.kind.as_str() {
        "file" => {
            let path = PathBuf::from(&row.target);
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "File".into());
            let exists = path.exists();
            Some((
                Slot::Code,
                label,
                Target::File {
                    path,
                    line: row.line.unwrap_or(1).max(1) as usize,
                },
                exists,
            ))
        }
        "project" => {
            let path = PathBuf::from(&row.target);
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "Project".into());
            let is_dir = path.is_dir();
            Some((Slot::Project, label, Target::Directory { path }, is_dir))
        }
        "video" => {
            // Labelled with the timestamp, not the URL — `▶ 06:54` says what pressing it
            // does; a truncated URL says nothing.
            let label = match row.timestamp_seconds {
                Some(seconds) => format!("▶ {}", format_timestamp(seconds)),
                None => "▶ Video".to_string(),
            };
            Some((
                Slot::Video,
                label,
                Target::Video {
                    url: row.target.clone(),
                    seconds: row.timestamp_seconds,
                },
                true,
            ))
        }
        "app" => {
            // An unresolvable `@app` still gets a button, disabled: the note names an
            // application that is not installed, and that is worth seeing.
            match apps.resolve(&row.target) {
                Some(app) => Some((
                    Slot::App,
                    app.name.clone(),
                    Target::App {
                        id: app.id.clone(),
                        name: app.name.clone(),
                    },
                    true,
                )),
                None => Some((
                    Slot::App,
                    row.target.clone(),
                    Target::App {
                        id: row.target.clone(),
                        name: row.target.clone(),
                    },
                    false,
                )),
            }
        }
        "url" => Some((
            Slot::Url,
            short_url(&row.target),
            Target::Url {
                url: row.target.clone(),
            },
            true,
        )),
        _ => None,
    }
}

/// `414` → `06:54`, `3723` → `1:02:03`.
pub fn format_timestamp(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// The host, for a URL button label. A full URL does not fit on a button.
fn short_url(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Expand `{path}`, `{line}`, `{url}`, and `{seconds}` into an argv.
///
/// Substitution happens **inside each element independently**, so a template element of
/// `+{line}` becomes `+41` while `{path}` becomes one argument no matter what it contains.
/// Nothing is re-split on whitespace, which is the whole point.
pub fn expand(template: &[String], target: &Target) -> Vec<String> {
    // A handler with no `{seconds}` placeholder — `xdg-open` — needs the timestamp put
    // back into the URL instead, and that rebuilding happens **here**, in trusted code,
    // from the parsed number. The stored URL stays canonical (spec §31).
    let rebuild_url = matches!(target, Target::Video { seconds: Some(_), .. })
        && !template.iter().any(|part| part.contains("{seconds}"));

    template
        .iter()
        .map(|element| {
            let mut out = element.clone();
            match target {
                Target::File { path, line } => {
                    out = out.replace("{path}", &path.display().to_string());
                    out = out.replace("{line}", &line.to_string());
                }
                Target::Directory { path } => {
                    out = out.replace("{path}", &path.display().to_string());
                }
                Target::Url { url } => {
                    out = out.replace("{url}", url);
                }
                Target::App { id, .. } => {
                    out = out.replace("{app}", id);
                }
                Target::Video { url, seconds } => {
                    let url = match (rebuild_url, seconds) {
                        (true, Some(seconds)) => with_timestamp(url, *seconds),
                        _ => url.clone(),
                    };
                    out = out.replace("{url}", &url);
                    out = out.replace("{seconds}", &seconds.unwrap_or(0).to_string());
                }
            }
            out
        })
        .collect()
}

/// Put a timestamp back into a URL for a handler that cannot take one separately.
fn with_timestamp(url: &str, seconds: u64) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}t={seconds}s")
}

/// Pick the opener template for a target.
///
/// Returns an owned argv rather than a borrow because two cases are *computed* rather than
/// configured: an app launches through `gtk-launch`, and a video may fall back to opening a
/// URL with the timestamp rebuilt into it.
pub fn template_for(openers: &Openers, target: &Target) -> Vec<String> {
    match target {
        Target::File { path, .. } => {
            let extension = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match extension.as_str() {
                "md" | "markdown" | "txt" => openers.markdown.clone(),
                _ => openers.text.clone(),
            }
        }
        Target::Directory { .. } => openers.directory.clone(),
        Target::Url { .. } => openers.url.clone(),
        Target::App { .. } => vec!["gtk-launch".into(), "{app}".into()],
        Target::Video { .. } => video_template(openers),
    }
}

/// Resolve the video handler: configured → `yclippy` → `mpv` → `xdg-open`.
///
/// A configured handler whose binary is not installed **falls back silently** rather than
/// failing (spec §51). Someone who configured `mpv` on one machine and syncs their config
/// to another should get a working video button, not an error.
///
/// `yclippy` sits above `mpv` in the fallback chain because it is the ecosystem's own
/// video surface: when `yclippy play {url} --at {seconds}` is on the PATH it understands
/// the timestamp directly and can trim and name the moment afterwards. Today this chain
/// is only walked by the daemon's `video_template` — yClippy is itself the destination,
/// not a caller — so the practical effect of probing `yclippy` first is that a fresh
/// `yclippy` install makes a `@video` action open in yclippy with no `[openers]` edit.
fn video_template(openers: &Openers) -> Vec<String> {
    if opener_is_installed(&openers.video) {
        return openers.video.clone();
    }

    if opener_is_installed(&["yclippy".to_string()]) {
        tracing::debug!("configured video handler is missing; using yclippy");
        return vec![
            "yclippy".into(),
            "play".into(),
            "{url}".into(),
            "--at".into(),
            "{seconds}".into(),
        ];
    }

    if opener_is_installed(&["mpv".to_string()]) {
        tracing::debug!("configured video handler is missing; using mpv");
        return vec!["mpv".into(), "--start={seconds}".into(), "{url}".into()];
    }

    // Last resort: the browser, with the timestamp put back into the URL by `expand`.
    tracing::debug!("no video player found; opening the URL");
    vec!["xdg-open".into(), "{url}".into()]
}

/// Run an action.
///
/// Spawns detached in its own process group so the opened editor survives the daemon
/// restarting, and with stdio null so a child that writes to stdout cannot interleave with
/// the daemon's own logging.
pub fn activate(openers: &Openers, target: &Target) -> Result<(), ActionError> {
    use std::os::unix::process::CommandExt as _;

    let template = template_for(openers, target);
    let argv = expand(&template, target);

    let Some((program, arguments)) = argv.split_first() else {
        return Err(ActionError::NotInstalled {
            program: "<empty opener>".into(),
        });
    };

    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Detach: without this the child is in the daemon's process group and a
        // `systemctl restart` closes the editor the user is typing in.
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ActionError::NotInstalled {
                    program: program.clone(),
                }
            } else {
                ActionError::Spawn {
                    program: program.clone(),
                    source,
                }
            }
        })
}

/// Is the program for this opener on `PATH`?
///
/// Used by `brainctl doctor`, so a misconfigured opener is found by asking rather than by
/// pressing `Alt+1` and having nothing happen.
pub fn opener_is_installed(template: &[String]) -> bool {
    let Some(program) = template.first() else {
        return false;
    };
    if program.contains('/') {
        return Path::new(program).exists();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(program).exists())
}

/// Every configured opener, for `brainctl doctor`.
pub fn all_openers(openers: &Openers) -> [(&'static str, &[String]); 5] {
    [
        ("markdown", &openers.markdown),
        ("text", &openers.text),
        ("directory", &openers.directory),
        ("url", &openers.url),
        ("video", &openers.video),
    ]
}

/// Expand `~` in a configured path. Re-exported so callers do not reach into config.
pub use config::expand_tilde;

#[cfg(test)]
mod tests {
    use super::*;

    fn openers() -> Openers {
        Openers::default()
    }

    #[test]
    fn a_path_with_spaces_quotes_and_a_dollar_survives_as_one_argument() {
        // The failure this prevents: shelling out and having the path split into four
        // arguments, or `$HOME` expanded by a shell that should never have seen it.
        let target = Target::File {
            path: PathBuf::from("/home/nabi/my notes/it's \"quoted\" $HOME.md"),
            line: 41,
        };
        let argv = expand(&openers().markdown, &target);

        assert_eq!(
            argv,
            vec![
                "ghostty",
                "-e",
                "nvim",
                "+41",
                "/home/nabi/my notes/it's \"quoted\" $HOME.md",
            ]
        );
        assert_eq!(argv.len(), 5, "the path was split into extra arguments");
    }

    #[test]
    fn substitution_happens_inside_an_element_not_by_replacing_it() {
        // `+{line}` has to become `+41`, one argument, which is what nvim's `+N` needs.
        let target = Target::File {
            path: PathBuf::from("/tmp/a.md"),
            line: 41,
        };
        let argv = expand(&openers().markdown, &target);
        assert!(argv.contains(&"+41".to_string()));
    }

    #[test]
    fn a_line_of_zero_becomes_one_because_editors_are_one_indexed() {
        let ranked = crate::tests::ranked_at_line(0);
        let actions = for_results(&ranked, &[], &DesktopIndex::default());
        assert!(matches!(
            actions[0].target,
            Target::File { line: 1, .. },
        ));
    }

    #[test]
    fn each_source_file_gets_one_action_in_rank_order() {
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md", "/tmp/a.md", "/tmp/b.md"]);
        let actions = for_results(&ranked, &[], &DesktopIndex::default());
        assert_eq!(actions.len(), 2, "the same file was offered twice");
        assert_eq!(actions[0].id, ActionId(1));
        assert_eq!(actions[1].id, ActionId(2));
    }

    #[test]
    fn no_more_actions_are_offered_than_there_are_keystrokes() {
        let paths: Vec<String> = (0..20).map(|n| format!("/tmp/{n}.md")).collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let actions = for_results(&crate::tests::ranked_paths(&borrowed), &[], &DesktopIndex::default());
        assert_eq!(actions.len(), 9, "Alt+1..9 is nine keystrokes");
    }

    #[test]
    fn a_missing_target_is_offered_disabled_rather_than_hidden() {
        let actions = for_results(
            &crate::tests::ranked_paths(&["/tmp/definitely-gone.md"]),
            &[],
            &DesktopIndex::default(),
        );
        assert_eq!(actions.len(), 1);
        assert!(!actions[0].enabled);
    }

    #[test]
    fn a_video_target_gets_its_timestamp() {
        let openers = Openers {
            video: vec!["mpv".into(), "--start={seconds}".into(), "{url}".into()],
            ..openers()
        };
        let target = Target::Video {
            url: "https://example.com/watch?v=ABC".into(),
            seconds: Some(414),
        };
        assert_eq!(
            expand(&openers.video, &target),
            vec!["mpv", "--start=414", "https://example.com/watch?v=ABC"]
        );
    }

    #[test]
    fn markdown_and_other_files_can_take_different_openers() {
        let openers = openers();
        let markdown = Target::File {
            path: PathBuf::from("/tmp/a.md"),
            line: 1,
        };
        let code = Target::File {
            path: PathBuf::from("/tmp/a.rs"),
            line: 1,
        };
        assert_eq!(template_for(&openers, &markdown), openers.markdown);
        assert_eq!(template_for(&openers, &code), openers.text);
    }

    #[test]
    fn a_missing_program_is_reported_as_not_installed() {
        let openers = Openers {
            markdown: vec!["definitely-not-a-real-program-xyz".into(), "{path}".into()],
            ..openers()
        };
        let target = Target::File {
            path: PathBuf::from("/tmp/a.md"),
            line: 1,
        };
        assert!(matches!(
            activate(&openers, &target),
            Err(ActionError::NotInstalled { .. })
        ));
        assert!(!opener_is_installed(&openers.markdown));
    }

    fn row(uid: &str, kind: &str, target: &str) -> ActionRow {
        ActionRow {
            section_uid: uid.into(),
            kind: kind.into(),
            target: target.into(),
            line: None,
            timestamp_seconds: None,
        }
    }

    #[test]
    fn buttons_are_ordered_by_kind_not_by_what_the_query_retrieved() {
        // `Alt+1` becomes muscle memory. If the video button is second for one query and
        // fourth for the next, that memory is worse than useless.
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md"]);
        let uid = ranked[0].hit.section_uid.clone();
        let declared = vec![
            row(&uid, "url", "https://example.com/docs"),
            row(&uid, "app", "blender"),
            row(&uid, "video", "https://youtu.be/ABC"),
            row(&uid, "file", "/tmp/code.rs"),
        ];

        let actions = for_results(&ranked, &declared, &DesktopIndex::default());
        let kinds: Vec<ActionKind> = actions.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [
                ActionKind::OpenFile,        // Note — always first
                ActionKind::OpenFile,        // Code
                ActionKind::OpenVideo,
                ActionKind::LaunchDesktopApp,
                ActionKind::OpenUrl,
            ]
        );
    }

    #[test]
    fn the_same_target_declared_on_three_sections_is_one_button() {
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md", "/tmp/b.md", "/tmp/c.md"]);
        let declared: Vec<ActionRow> = ranked
            .iter()
            .map(|entry| row(&entry.hit.section_uid, "video", "https://youtu.be/SAME"))
            .collect();

        let actions = for_results(&ranked, &declared, &DesktopIndex::default());
        let videos = actions
            .iter()
            .filter(|a| a.kind == ActionKind::OpenVideo)
            .count();
        assert_eq!(videos, 1, "the same video produced {videos} buttons");
    }

    #[test]
    fn a_video_button_says_its_timestamp_rather_than_its_url() {
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md"]);
        let mut declared = row(&ranked[0].hit.section_uid, "video", "https://youtu.be/ABC");
        declared.timestamp_seconds = Some(414);

        let actions = for_results(&ranked, &[declared], &DesktopIndex::default());
        let video = actions
            .iter()
            .find(|a| a.kind == ActionKind::OpenVideo)
            .unwrap();
        assert_eq!(video.label, "▶ 06:54");
    }

    #[test]
    fn timestamps_format_the_way_a_player_shows_them() {
        assert_eq!(format_timestamp(414), "06:54");
        assert_eq!(format_timestamp(59), "00:59");
        assert_eq!(format_timestamp(3723), "1:02:03");
    }

    #[test]
    fn an_uninstalled_app_is_a_disabled_button_rather_than_a_missing_one() {
        // The note names something that is not installed. Hiding the button hides the
        // problem; showing it disabled says the note needs fixing.
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md"]);
        let declared = row(&ranked[0].hit.section_uid, "app", "not-installed-anywhere");

        let actions = for_results(&ranked, &[declared], &DesktopIndex::default());
        let app = actions
            .iter()
            .find(|a| a.kind == ActionKind::LaunchDesktopApp)
            .expect("the button was dropped entirely");
        assert!(!app.enabled);
    }

    #[test]
    fn an_app_launches_through_gtk_launch_by_id() {
        // Not through the Exec line: field codes (%f, %U, %i) have per-code expansion
        // rules, and hand-parsing them breaks specific applications quietly.
        let target = Target::App {
            id: "com.obsproject.Studio".into(),
            name: "OBS Studio".into(),
        };
        let template = template_for(&openers(), &target);
        assert_eq!(
            expand(&template, &target),
            vec!["gtk-launch", "com.obsproject.Studio"]
        );
    }

    #[test]
    fn a_handler_without_a_seconds_placeholder_gets_the_timestamp_in_the_url() {
        // `xdg-open` cannot take a start time as an argument, so trusted code rebuilds the
        // URL — which is why the stored URL is kept canonical rather than round-tripped.
        let target = Target::Video {
            url: "https://youtu.be/ABC".into(),
            seconds: Some(414),
        };
        assert_eq!(
            expand(&["xdg-open".into(), "{url}".into()], &target),
            vec!["xdg-open", "https://youtu.be/ABC?t=414s"]
        );

        // A URL that already has a query keeps it.
        let target = Target::Video {
            url: "https://www.youtube.com/watch?v=ABC".into(),
            seconds: Some(60),
        };
        assert_eq!(
            expand(&["xdg-open".into(), "{url}".into()], &target),
            vec!["xdg-open", "https://www.youtube.com/watch?v=ABC&t=60s"]
        );
    }

    #[test]
    fn a_handler_with_a_seconds_placeholder_leaves_the_url_alone() {
        let target = Target::Video {
            url: "https://youtu.be/ABC".into(),
            seconds: Some(414),
        };
        assert_eq!(
            expand(
                &["mpv".into(), "--start={seconds}".into(), "{url}".into()],
                &target
            ),
            vec!["mpv", "--start=414", "https://youtu.be/ABC"]
        );
    }

    #[test]
    fn a_configured_video_handler_that_is_not_installed_falls_back() {
        // Syncing a config between machines should not leave a dead button (spec §51).
        let openers = Openers {
            video: vec!["definitely-not-a-player-xyz".into(), "{url}".into()],
            ..openers()
        };
        let template = video_template(&openers);
        assert_ne!(template[0], "definitely-not-a-player-xyz");
        // Whatever the machine has, the answer is one of the chain's rungs and
        // always carries a `{url}` for `expand` to fill.
        assert!(["yclippy", "mpv", "xdg-open"].contains(&template[0].as_str()));
        assert!(template.iter().any(|part| part.contains("{url}")));
    }

    /// Installing the ecosystem's own video surface should be enough to make it
    /// the player, without editing a configuration file.
    #[test]
    fn yclippy_is_preferred_over_mpv_when_nothing_is_configured() {
        let openers = Openers {
            video: vec!["definitely-not-a-player-xyz".into(), "{url}".into()],
            ..openers()
        };
        let template = video_template(&openers);
        if opener_is_installed(&["yclippy".to_string()]) {
            assert_eq!(template[0], "yclippy");
        }
    }

    #[test]
    fn a_declared_file_action_carries_its_line() {
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md"]);
        let mut declared = row(&ranked[0].hit.section_uid, "file", "/tmp/src/smoothing.rs");
        declared.line = Some(41);

        let actions = for_results(&ranked, &[declared], &DesktopIndex::default());
        let code = &actions[1];
        assert_eq!(code.label, "smoothing.rs");
        assert!(matches!(code.target, Target::File { line: 41, .. }));
    }

    #[test]
    fn a_real_program_is_found_on_path() {
        assert!(opener_is_installed(&["sh".to_string()]));
        assert!(opener_is_installed(&["/bin/sh".to_string()]));
        assert!(!opener_is_installed(&["/bin/definitely-not-here".to_string()]));
        assert!(!opener_is_installed(&[]));
    }
}
