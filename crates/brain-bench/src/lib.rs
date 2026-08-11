//! Retrieval quality as a number you can move on purpose.
//!
//! Stage 7. Everything before this stage was decided by impression: whether graph expansion
//! helps, whether `bm25_heading = 8.0` is right, whether the stopword list earns its place.
//! Those are all answerable, and none of them are answerable without this.
//!
//! Three decisions shape the design:
//!
//! **It runs in-process, not through the daemon.** A benchmark that needs a running daemon
//! is one you will not run. This opens the vault directly, so a sweep can rebuild the config
//! forty times in a loop without restarting anything.
//!
//! **It measures retrieval only** (spec §57). Generation quality has no honest automated
//! metric at this scale — an LLM judge over forty items is noise — so it gets a manual
//! held-out set instead, and mixing the two here would produce a number that moves for
//! reasons you cannot attribute.
//!
//! **The label is a `section_uid`.** Not a path, not a rank: the identity `yalive`,
//! `yGraphy`, and the `relations` table already share. That means a question labelled from a
//! real answer stays valid when the note is edited or moved, and only stops being valid when
//! the section itself is renamed — which is exactly when the label *should* be revisited.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use brain_core::Config;
use brain_index::Index;
use serde::{Deserialize, Serialize};

/// One benchmark question and the sections that should answer it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Question {
    pub question: String,
    /// `section_uid`s that count as correct. Any one of them in the top *k* is a hit.
    pub expected: Vec<String>,
    /// What was focused when this was asked (Stage 4). Optional; questions exported from
    /// real usage carry it, hand-written ones usually do not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<QuestionContext>,
    /// Free-text note about why this question is in the set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QuestionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

impl QuestionContext {
    fn to_desktop(&self) -> brain_proto::DesktopContext {
        brain_proto::DesktopContext {
            wm_class: self.app.clone(),
            cwd: self.cwd.clone(),
            ..Default::default()
        }
    }
}

/// What one question scored.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Outcome {
    pub question: String,
    /// 1-based rank of the first expected section, or `None` if it never appeared.
    pub rank: Option<usize>,
    pub elapsed_ms: f64,
    pub returned: usize,
}

/// Aggregate scores over a question set.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Report {
    pub questions: usize,
    pub recall_at_1: f64,
    pub recall_at_3: f64,
    pub recall_at_5: f64,
    /// Mean reciprocal rank. Rewards being *first* rather than merely present, which is what
    /// matters when the dock shows one primary source.
    pub mrr: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub outcomes: Vec<Outcome>,
}

impl Report {
    fn from(outcomes: Vec<Outcome>) -> Self {
        let questions = outcomes.len();
        if questions == 0 {
            return Self::default();
        }

        let recall_at = |k: usize| {
            outcomes
                .iter()
                .filter(|outcome| outcome.rank.is_some_and(|rank| rank <= k))
                .count() as f64
                / questions as f64
        };

        let mrr = outcomes
            .iter()
            .map(|outcome| outcome.rank.map_or(0.0, |rank| 1.0 / rank as f64))
            .sum::<f64>()
            / questions as f64;

        let mut times: Vec<f64> = outcomes.iter().map(|outcome| outcome.elapsed_ms).collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        Self {
            questions,
            recall_at_1: recall_at(1),
            recall_at_3: recall_at(3),
            recall_at_5: recall_at(5),
            mrr,
            p50_ms: percentile(&times, 0.50),
            p95_ms: percentile(&times, 0.95),
            outcomes,
        }
    }

    /// The table `brain-bench` prints.
    pub fn render(&self, label: &str) -> String {
        format!(
            "{label}  n={n}\n\
             Recall@1  {r1:.2}      MRR      {mrr:.2}\n\
             Recall@3  {r3:.2}      p50   {p50:6.1} ms\n\
             Recall@5  {r5:.2}      p95   {p95:6.1} ms",
            n = self.questions,
            r1 = self.recall_at_1,
            r3 = self.recall_at_3,
            r5 = self.recall_at_5,
            mrr = self.mrr,
            p50 = self.p50_ms,
            p95 = self.p95_ms,
        )
    }

    /// Questions whose rank changed against a previous run.
    ///
    /// The aggregate alone is not enough to accept a change: a mean that improves by 2%
    /// while breaking five questions you care about is not an improvement, and only a
    /// per-question comparison shows that.
    pub fn diff(&self, previous: &Report) -> Vec<Movement> {
        let mut moved = Vec::new();
        for outcome in &self.outcomes {
            let Some(before) = previous
                .outcomes
                .iter()
                .find(|other| other.question == outcome.question)
            else {
                continue;
            };
            if before.rank != outcome.rank {
                moved.push(Movement {
                    question: outcome.question.clone(),
                    before: before.rank,
                    after: outcome.rank,
                });
            }
        }
        // Regressions first: they are the reason to look.
        moved.sort_by_key(|movement| movement.delta());
        moved.reverse();
        moved
    }
}

/// A question whose rank changed between runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Movement {
    pub question: String,
    pub before: Option<usize>,
    pub after: Option<usize>,
}

impl Movement {
    /// Positive means it got worse. Missing counts as the bottom of the list.
    fn delta(&self) -> i64 {
        const MISSING: i64 = 1_000;
        self.after.map_or(MISSING, |rank| rank as i64)
            - self.before.map_or(MISSING, |rank| rank as i64)
    }

    pub fn render(&self) -> String {
        let show = |rank: Option<usize>| rank.map_or("—".to_string(), |rank| rank.to_string());
        let arrow = if self.delta() > 0 { "worse" } else { "better" };
        format!(
            "  {arrow:<7} {} → {}   {}",
            show(self.before),
            show(self.after),
            self.question
        )
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Load a question set.
pub fn load_questions(path: &Path) -> Result<Vec<Question>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Run every question against a vault and score the result.
///
/// Reindexes first so the numbers describe the vault as it is on disk, not as some previous
/// run left the index.
pub async fn run(vault: &Path, config: &Config, questions: &[Question]) -> Result<Report> {
    let index = Index::open(vault)?;
    index.reindex().await?;

    let mut outcomes = Vec::with_capacity(questions.len());
    for question in questions {
        outcomes.push(ask(&index, config, question).await?);
    }
    Ok(Report::from(outcomes))
}

async fn ask(index: &Index, config: &Config, question: &Question) -> Result<Outcome> {
    let started = std::time::Instant::now();

    let mut retrieval = brain_engine::retrieve(index, &config.search, &question.question).await?;
    brain_engine::boost_heading_matches(&config.search, &question.question, &mut retrieval.results);

    if config.context.enabled
        && let Some(context) = &question.context
    {
        brain_engine::apply_context(
            &config.search,
            &context.to_desktop(),
            &config.context.aliases,
            &question.question,
            &mut retrieval.results,
        );
    }

    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    // The first position holding *any* expected section. Several sections can legitimately
    // answer one question, and demanding a specific one would punish a correct answer for
    // picking a different valid source.
    let rank = retrieval
        .results
        .iter()
        .position(|entry| question.expected.contains(&entry.hit.section_uid))
        .map(|index| index + 1);

    Ok(Outcome {
        question: question.question.clone(),
        rank,
        elapsed_ms,
        returned: retrieval.results.len(),
    })
}

/// Override one dotted config key, as a sweep does.
///
/// Goes through TOML rather than matching on field names: the config is already
/// `Serialize + Deserialize`, so this stays correct as fields are added, and a typo'd key is
/// rejected by the same `deny_unknown_fields` that guards the real config file. That is the
/// whole reason spec §19 insists the weights live in config rather than in code.
pub fn with_override(config: &Config, key: &str, value: &str) -> Result<Config> {
    let mut document: toml::Value = toml::Value::try_from(config)?;

    let parts: Vec<&str> = key.split('.').collect();
    let (last, path) = parts.split_last().context("empty config key")?;

    let mut cursor = &mut document;
    for part in path {
        cursor = cursor
            .get_mut(*part)
            .with_context(|| format!("no config section `{part}` in `{key}`"))?;
    }

    let table = cursor
        .as_table_mut()
        .with_context(|| format!("`{key}` does not name a value"))?;
    // Parsed as TOML so `8.0` is a float, `true` a bool, and `"x"` a string, without this
    // needing to know the field's type.
    let parsed: toml::Value = value
        .parse()
        .with_context(|| format!("`{value}` is not a TOML value"))?;
    table.insert((*last).to_string(), parsed);

    document
        .try_into()
        .with_context(|| format!("`{key} = {value}` is not valid for this config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(question: &str, rank: Option<usize>) -> Outcome {
        Outcome {
            question: question.into(),
            rank,
            elapsed_ms: 1.0,
            returned: 5,
        }
    }

    #[test]
    fn recall_counts_a_hit_anywhere_in_the_top_k() {
        let report = Report::from(vec![
            outcome("a", Some(1)),
            outcome("b", Some(3)),
            outcome("c", Some(9)),
            outcome("d", None),
        ]);

        assert_eq!(report.recall_at_1, 0.25);
        assert_eq!(report.recall_at_3, 0.50);
        assert_eq!(report.recall_at_5, 0.50);
    }

    #[test]
    fn mrr_rewards_being_first_rather_than_merely_present() {
        // The dock shows one primary source, so rank 1 and rank 5 are very different
        // outcomes that recall@5 scores identically.
        let first = Report::from(vec![outcome("a", Some(1))]);
        let fifth = Report::from(vec![outcome("a", Some(5))]);

        assert_eq!(first.recall_at_5, fifth.recall_at_5);
        assert!(first.mrr > fifth.mrr);
        assert_eq!(first.mrr, 1.0);
        assert_eq!(fifth.mrr, 0.2);
    }

    #[test]
    fn a_question_that_never_matched_scores_zero_rather_than_being_skipped() {
        // Dropping misses would make a set that answers one question perfectly look better
        // than one that answers nine well and misses one.
        let report = Report::from(vec![outcome("a", Some(1)), outcome("b", None)]);
        assert_eq!(report.questions, 2);
        assert_eq!(report.mrr, 0.5);
    }

    #[test]
    fn an_empty_set_does_not_divide_by_zero() {
        let report = Report::from(Vec::new());
        assert_eq!(report.questions, 0);
        assert_eq!(report.mrr, 0.0);
    }

    #[test]
    fn the_diff_puts_regressions_first() {
        // An aggregate that improves while breaking a question you care about is not an
        // improvement, and this ordering is what surfaces that.
        let before = Report::from(vec![
            outcome("improved", Some(5)),
            outcome("broke", Some(1)),
            outcome("same", Some(2)),
        ]);
        let after = Report::from(vec![
            outcome("improved", Some(1)),
            outcome("broke", None),
            outcome("same", Some(2)),
        ]);

        let moved = after.diff(&before);
        assert_eq!(moved.len(), 2, "unchanged questions were reported");
        assert_eq!(moved[0].question, "broke");
        assert_eq!(moved[1].question, "improved");
    }

    #[test]
    fn a_sweep_override_replaces_exactly_one_value() {
        let config = Config::default();
        let swept = with_override(&config, "search.bm25_heading", "12.0").unwrap();

        assert_eq!(swept.search.bm25_heading, 12.0);
        // Everything else survives.
        assert_eq!(swept.search.bm25_body, config.search.bm25_body);
        assert_eq!(swept.search.fusion.k, config.search.fusion.k);
    }

    #[test]
    fn a_nested_override_reaches_into_subtables() {
        let config = Config::default();
        let swept = with_override(&config, "search.graph.max_hops", "3").unwrap();
        assert_eq!(swept.search.graph.max_hops, 3);

        let swept = with_override(&config, "search.graph.enabled", "false").unwrap();
        assert!(!swept.search.graph.enabled);
    }

    #[test]
    fn a_misspelled_sweep_key_is_refused_rather_than_silently_ignored() {
        // Sweeping a key that does not exist would report that the parameter has no effect,
        // which is true and completely misleading.
        let config = Config::default();
        assert!(with_override(&config, "search.bm25_headding", "12.0").is_err());
        assert!(with_override(&config, "nonsense.key", "1").is_err());
    }

    #[test]
    fn question_sets_round_trip_through_yaml() {
        let questions = vec![Question {
            question: "how do I schedule a nightly backup?".into(),
            expected: vec!["pg-backup-cron#root".into()],
            context: Some(QuestionContext {
                app: Some("ghostty".into()),
                cwd: Some(PathBuf::from("/tmp/game")),
            }),
            note: None,
        }];

        let yaml = serde_yaml::to_string(&questions).unwrap();
        let parsed: Vec<Question> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed[0].expected, questions[0].expected);
        assert_eq!(parsed[0].context.as_ref().unwrap().app.as_deref(), Some("ghostty"));
    }
}
