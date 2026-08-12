//! Keyboard actions.
//!
//! Toolkit-independent by design, and it carried across the iced port
//! unchanged. The view reports *named* shortcuts rather than raw keys, so the
//! binding table lives here in one place. Spec §5 says these must be
//! configurable; parsing names from config is a small change on top of this,
//! and none of the call sites move when it happens.

/// What a key press asked for.
///
/// Named `Command`, not `Action`: `brain-proto::ActionView` already means "a
/// button that jumps somewhere", and these are keyboard commands. Two different
/// things called Action in one file is a bug waiting to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ClearQuery,
    CopyAnswer,
    ShowSources,
    EditAnswer,
    Retry,
    HistoryPrevious,
    HistoryNext,
    SelectNextAction,
    SelectPreviousAction,
    /// `Alt+1..9`; carries a zero-based index.
    Activate(usize),
    /// Edit the answer on screen into a correction (spec §4's Correction state).
    Correct,
    /// Save the edited correction.
    SaveCorrection,
    /// Mark the answer on screen good or bad.
    ///
    /// One keystroke, and after a fortnight of ordinary use the daemon has a labelled
    /// retrieval benchmark built from the questions actually asked (`PLAN.md` §6.3).
    Rate(bool),
    /// Open or close the graph panel under the answer.
    ///
    /// The daemon owns the flag and relays the visibility to the dock, exactly as it
    /// does for the dock itself — which is what keeps the shortcut stateless and
    /// lets an i3 binding be a one-liner.
    ToggleGraph,
}

impl Command {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "clear" => Self::ClearQuery,
            "copy-answer" => Self::CopyAnswer,
            "show-sources" => Self::ShowSources,
            "edit-answer" => Self::EditAnswer,
            "retry" => Self::Retry,
            "history-previous" => Self::HistoryPrevious,
            "history-next" => Self::HistoryNext,
            "action-next" => Self::SelectNextAction,
            "action-previous" => Self::SelectPreviousAction,
            "correct" => Self::Correct,
            "save-correction" => Self::SaveCorrection,
            "rate-good" => Self::Rate(true),
            "rate-bad" => Self::Rate(false),
            "toggle-graph" => Self::ToggleGraph,
            other => {
                // `action-1` .. `action-9`, one-based in the UI because that is
                // what the button labels show.
                let n: usize = other.strip_prefix("action-")?.parse().ok()?;
                Self::Activate(n.checked_sub(1)?)
            }
        })
    }
}

/// Recently submitted queries, newest last.
///
/// Up/Down walk backwards through this. Kept in the dock rather than the daemon
/// because it is presentation state — the daemon's `query_history` table
/// (Stage 6) is a different thing with a different lifetime.
pub struct History {
    entries: Vec<String>,
    /// How far back we have walked. `None` means "at the live entry".
    cursor: Option<usize>,
    /// What the user had typed before starting to walk, so Down can restore it.
    draft: String,
}

const MAX_ENTRIES: usize = 100;

impl History {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            draft: String::new(),
        }
    }

    pub fn push(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        // Asking the same thing twice in a row should not need two Ups to walk
        // past.
        if self.entries.last().map(String::as_str) == Some(query) {
            self.cursor = None;
            return;
        }
        self.entries.push(query.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.cursor = None;
    }

    /// Older entry, or `None` at the beginning.
    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.cursor = Some(next);
        self.entries.get(next).cloned()
    }

    /// Newer entry, or the original draft once past the end.
    pub fn next(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 >= self.entries.len() {
            self.cursor = None;
            return Some(std::mem::take(&mut self.draft));
        }
        self.cursor = Some(index + 1);
        self.entries.get(index + 1).cloned()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_parse() {
        assert_eq!(Command::parse("clear"), Some(Command::ClearQuery));
        assert_eq!(Command::parse("action-1"), Some(Command::Activate(0)));
        assert_eq!(Command::parse("action-9"), Some(Command::Activate(8)));
        assert_eq!(Command::parse("toggle-graph"), Some(Command::ToggleGraph));
        assert_eq!(Command::parse("nonsense"), None);
        // `action-0` would underflow a zero-based index; the UI never sends it.
        assert_eq!(Command::parse("action-0"), None);
    }

    #[test]
    fn history_walks_backwards_and_forwards() {
        let mut history = History::new();
        history.push("first");
        history.push("second");

        assert_eq!(history.previous(""), Some("second".into()));
        assert_eq!(history.previous(""), Some("first".into()));
        assert_eq!(history.previous(""), None, "should stop at the oldest");

        assert_eq!(history.next(), Some("second".into()));
        assert_eq!(history.next(), Some(String::new()), "restores the draft");
    }

    #[test]
    fn walking_back_preserves_what_was_typed() {
        let mut history = History::new();
        history.push("old query");

        assert_eq!(history.previous("half-typed"), Some("old query".into()));
        assert_eq!(
            history.next(),
            Some("half-typed".into()),
            "Down must give back what the user was writing"
        );
    }

    #[test]
    fn consecutive_duplicates_are_not_stored_twice() {
        let mut history = History::new();
        history.push("same");
        history.push("same");
        assert_eq!(history.previous(""), Some("same".into()));
        assert_eq!(history.previous(""), None);
    }

    #[test]
    fn blank_queries_are_ignored() {
        let mut history = History::new();
        history.push("   ");
        assert_eq!(history.previous(""), None);
    }

    #[test]
    fn history_is_bounded() {
        let mut history = History::new();
        for i in 0..(MAX_ENTRIES + 50) {
            history.push(&format!("query {i}"));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        // The oldest entries were dropped, not the newest.
        assert_eq!(
            history.previous(""),
            Some(format!("query {}", MAX_ENTRIES + 49))
        );
    }
}
