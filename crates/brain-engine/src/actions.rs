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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use brain_core::config::Openers;
use brain_core::{ActionId, config};
use brain_proto::{ActionKind, ActionView};

use crate::Ranked;

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
    Video { url: String, seconds: u64 },
    Directory { path: PathBuf },
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
            Target::Video { url, seconds } => format!("{url} @{seconds}s"),
            Target::Directory { path } => display_path(path),
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

/// Build the actions offered for a ranked result set.
///
/// One `Open note` per distinct source, in rank order — the primary source first, because
/// `Alt+1` is the keystroke the whole product is shaped around.
pub fn for_results(results: &[Ranked]) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut seen: Vec<&Path> = Vec::new();

    for entry in results {
        let path = entry.hit.path.as_path();
        if seen.contains(&path) {
            continue;
        }
        seen.push(path);

        let id = ActionId(actions.len() as i64 + 1);
        actions.push(Action {
            id,
            kind: ActionKind::OpenFile,
            // The heading, not the filename: it is what the user was reading on screen.
            label: if entry.hit.heading.is_empty() {
                entry.hit.note_title.clone()
            } else {
                entry.hit.heading.clone()
            },
            target: Target::File {
                path: path.to_path_buf(),
                line: entry.hit.start_line.max(1),
            },
            // A path that vanished between indexing and now is the normal way this goes
            // wrong, and it is worth showing rather than hiding.
            enabled: path.exists(),
        });

        if actions.len() >= 9 {
            // `Alt+1` … `Alt+9`; a tenth has no keystroke.
            break;
        }
    }

    actions
}

/// Expand `{path}`, `{line}`, `{url}`, and `{seconds}` into an argv.
///
/// Substitution happens **inside each element independently**, so a template element of
/// `+{line}` becomes `+41` while `{path}` becomes one argument no matter what it contains.
/// Nothing is re-split on whitespace, which is the whole point.
pub fn expand(template: &[String], target: &Target) -> Vec<String> {
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
                Target::Video { url, seconds } => {
                    out = out.replace("{url}", url);
                    out = out.replace("{seconds}", &seconds.to_string());
                }
            }
            out
        })
        .collect()
}

/// Pick the opener template for a target.
pub fn template_for<'a>(openers: &'a Openers, target: &Target) -> &'a [String] {
    match target {
        Target::File { path, .. } => {
            let extension = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match extension.as_str() {
                "md" | "markdown" => &openers.markdown,
                _ => &openers.text,
            }
        }
        Target::Directory { .. } => &openers.directory,
        Target::Url { .. } => &openers.url,
        Target::Video { .. } => &openers.video,
    }
}

/// Run an action.
///
/// Spawns detached in its own process group so the opened editor survives the daemon
/// restarting, and with stdio null so a child that writes to stdout cannot interleave with
/// the daemon's own logging.
pub fn activate(openers: &Openers, target: &Target) -> Result<(), ActionError> {
    use std::os::unix::process::CommandExt as _;

    let template = template_for(openers, target);
    let argv = expand(template, target);

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
        let actions = for_results(&ranked);
        assert!(matches!(
            actions[0].target,
            Target::File { line: 1, .. },
        ));
    }

    #[test]
    fn each_source_file_gets_one_action_in_rank_order() {
        let ranked = crate::tests::ranked_paths(&["/tmp/a.md", "/tmp/a.md", "/tmp/b.md"]);
        let actions = for_results(&ranked);
        assert_eq!(actions.len(), 2, "the same file was offered twice");
        assert_eq!(actions[0].id, ActionId(1));
        assert_eq!(actions[1].id, ActionId(2));
    }

    #[test]
    fn no_more_actions_are_offered_than_there_are_keystrokes() {
        let paths: Vec<String> = (0..20).map(|n| format!("/tmp/{n}.md")).collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let actions = for_results(&crate::tests::ranked_paths(&borrowed));
        assert_eq!(actions.len(), 9, "Alt+1..9 is nine keystrokes");
    }

    #[test]
    fn a_missing_target_is_offered_disabled_rather_than_hidden() {
        let actions = for_results(&crate::tests::ranked_paths(&["/tmp/definitely-gone.md"]));
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
            seconds: 414,
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
        assert_eq!(template_for(&openers, &markdown), &openers.markdown[..]);
        assert_eq!(template_for(&openers, &code), &openers.text[..]);
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

    #[test]
    fn a_real_program_is_found_on_path() {
        assert!(opener_is_installed(&["sh".to_string()]));
        assert!(opener_is_installed(&["/bin/sh".to_string()]));
        assert!(!opener_is_installed(&["/bin/definitely-not-here".to_string()]));
        assert!(!opener_is_installed(&[]));
    }
}
