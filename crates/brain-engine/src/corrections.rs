//! Correcting an answer once, and having it stick.
//!
//! Spec §33. This is how the assistant learns *immediately*, and note what it is not:
//! fine-tuning. Facts belong in the knowledge store, corrections belong here, and only
//! behaviour and style would ever justify a LoRA (spec §35).
//!
//! Two lookup paths, deliberately different:
//!
//! - **Exact normalised match → return the stored answer verbatim, instantly.** No model
//!   call. Asking the identical question again should give the identical corrected answer,
//!   and paying 600 ms to have a 1.7B model paraphrase text you already approved is worse
//!   in every dimension.
//! - **Fuzzy match → inject into the prompt as an authoritative source**, and let the model
//!   adapt it. Returning stored text for a *differently* phrased question gives a canned
//!   answer to something that was not quite asked; letting the model use it as a basis keeps
//!   the answer responsive at the cost of one generation.
//!
//! **The gap worth naming:** the plan makes Stage 5 embeddings a prerequisite, because
//! cosine similarity over question embeddings is what catches a genuine paraphrase. That is
//! deferred, so the fuzzy path here is lexical — FTS5 over the stored questions, which §6.3
//! lists as the cheap parallel path. It fires on shared vocabulary and misses a paraphrase
//! that shares none. Whether that gap matters is a question for the benchmark, and it is
//! precisely the sort of evidence Phase G wants before embeddings are built.

use std::collections::HashSet;

/// A stored correction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub id: i64,
    pub question: String,
    pub normalized: String,
    pub good_answer: String,
    /// Sections that were in the context pack when this was saved, with the content hash
    /// each had at the time.
    pub sources: Vec<(String, String)>,
    /// A source has changed since the correction was saved, so it *might* be obsolete.
    pub stale: bool,
}

/// How a correction was found, which decides how it is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// Identical question. Return the stored answer; do not call the model.
    Exact,
    /// Similar wording. Inject into the prompt and let the model adapt it.
    Fuzzy,
}

/// Normalise a question for matching.
///
/// Lowercase, strip punctuation, collapse whitespace, and drop the stopwords the FTS layer
/// already ignores. "How do I mirror bones?" and "how do i mirror bones" have to land on the
/// same key, or the exact-match path fires almost never and the feature reads as broken.
///
/// Stopwords are dropped so that "how do I deploy" and "how should I deploy" match: the
/// difference between them is not a difference in what was asked.
pub fn normalize(question: &str) -> String {
    let mut words: Vec<String> = question
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty() && !yalive::search::is_stopword(word))
        .collect();

    // A question made entirely of stopwords ("how do I") still needs a key, or every such
    // question collides on the empty string.
    if words.is_empty() {
        words = question
            .split_whitespace()
            .map(|word| word.to_lowercase())
            .collect();
    }
    words.join(" ")
}

/// Score how well a stored correction matches a question, in `0.0..=1.0`.
///
/// Jaccard over normalised word sets. Crude next to cosine similarity on embeddings, and
/// honestly so: it measures shared vocabulary, which is a decent proxy for "the same
/// question reworded" and no proxy at all for "the same question in different words".
pub fn similarity(left: &str, right: &str) -> f32 {
    let left: HashSet<&str> = left.split_whitespace().collect();
    let right: HashSet<&str> = right.split_whitespace().collect();

    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left.intersection(&right).count() as f32;
    let total = left.union(&right).count() as f32;
    shared / total
}

/// Pick the correction to apply, if any.
///
/// An exact normalised match always wins, however good a fuzzy score another correction
/// scores: the user asked this exact thing and approved this exact answer.
pub fn best<'a>(
    question: &str,
    corrections: &'a [Correction],
    threshold: f32,
) -> Option<(&'a Correction, Match)> {
    let normalized = normalize(question);

    if let Some(exact) = corrections
        .iter()
        .find(|correction| correction.normalized == normalized)
    {
        return Some((exact, Match::Exact));
    }

    let (best, score) = corrections
        .iter()
        .map(|correction| {
            let mut score = similarity(&normalized, &correction.normalized);
            // A stale correction still applies — its source changed, so it *might* be
            // obsolete, but silently dropping an explicit user correction is worse than
            // showing a possibly-outdated one (spec §33). It just has to lose a tie.
            if correction.stale {
                score *= 0.8;
            }
            (correction, score)
        })
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))?;

    (score >= threshold).then_some((best, Match::Fuzzy))
}

/// The block injected ahead of the retrieved sources.
///
/// Marked explicitly as authoritative so the model prefers it over the notes below it, and
/// marked *differently* when stale so it is a strong hint rather than a command.
pub fn prompt_block(correction: &Correction) -> String {
    if correction.stale {
        format!(
            "USER-CONFIRMED CORRECTION (a source has changed since this was confirmed; \
             prefer it only if the sources below do not contradict it):\n{}\n\n",
            correction.good_answer
        )
    } else {
        format!(
            "USER-CONFIRMED CORRECTION (authoritative, overrides the sources below):\n{}\n\n",
            correction.good_answer
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn correction(question: &str, answer: &str) -> Correction {
        Correction {
            id: 1,
            question: question.into(),
            normalized: normalize(question),
            good_answer: answer.into(),
            sources: Vec::new(),
            stale: false,
        }
    }

    #[test]
    fn punctuation_and_case_do_not_defeat_the_exact_match() {
        // If these produce different keys the exact-match path fires almost never, and the
        // feature reads as broken rather than as approximate.
        assert_eq!(normalize("How do I mirror bones?"), normalize("how do i mirror bones"));
        assert_eq!(normalize("  Deploy,  now!  "), normalize("deploy now"));
    }

    #[test]
    fn a_difference_of_only_stopwords_is_not_a_different_question() {
        assert_eq!(normalize("how do I deploy"), normalize("how should I deploy"));
    }

    #[test]
    fn a_question_of_only_stopwords_still_gets_a_key() {
        // Otherwise every such question collides on the empty string and the first
        // correction ever saved answers all of them.
        assert!(!normalize("how do I").is_empty());
        assert_ne!(normalize("how do I"), normalize("what is it"));
    }

    #[test]
    fn an_exact_match_wins_over_a_better_scoring_fuzzy_one() {
        let stored = vec![
            correction("how do I deploy", "Use the container rollout."),
            correction(
                "how do I deploy the container image to production",
                "Longer answer.",
            ),
        ];

        let (found, kind) = best("how do I deploy", &stored, 0.5).unwrap();
        assert_eq!(kind, Match::Exact);
        assert_eq!(found.good_answer, "Use the container rollout.");
    }

    #[test]
    fn a_reworded_question_matches_fuzzily() {
        // Extra content words, not just extra stopwords — a difference of stopwords alone
        // normalises to the *same* key and is an exact match, which the test above pins.
        let stored = vec![correction(
            "how do I deploy the container",
            "Use the container rollout.",
        )];

        let (found, kind) = best(
            "how do I deploy the container image to production?",
            &stored,
            0.4,
        )
        .unwrap();
        assert_eq!(kind, Match::Fuzzy);
        assert_eq!(found.good_answer, "Use the container rollout.");
    }

    #[test]
    fn an_unrelated_question_does_not_pick_up_a_correction() {
        // The leak that would make this feature actively harmful: a correction about
        // deployment turning up in an answer about backups.
        let stored = vec![correction(
            "how do I deploy the container",
            "Use the container rollout.",
        )];
        assert!(best("how long are backups kept", &stored, 0.5).is_none());
    }

    #[test]
    fn a_genuine_paraphrase_with_no_shared_words_is_missed() {
        // Recorded rather than hidden: this is exactly what Stage 5 embeddings would catch,
        // and the reason the plan lists them as this stage's prerequisite. Lexical matching
        // has no way to connect these.
        let stored = vec![correction(
            "how do I ship a new version",
            "Use the container rollout.",
        )];
        assert!(best("what is the release process", &stored, 0.5).is_none());
    }

    #[test]
    fn a_stale_correction_still_applies_but_loses_a_tie() {
        let mut fresh = correction("how do I deploy the container", "Fresh.");
        fresh.id = 1;
        let mut stale = correction("how do I deploy the container image", "Stale.");
        stale.id = 2;
        stale.stale = true;

        // Alone, it still applies — dropping an explicit correction silently is worse than
        // showing a possibly-outdated one.
        let only_stale = [stale.clone()];
        let (found, _) = best("how do I deploy a container image", &only_stale, 0.4).unwrap();
        assert_eq!(found.good_answer, "Stale.");

        // Against a fresh one of comparable score, it loses.
        let both = [stale, fresh];
        let (found, _) = best("how do I deploy the container", &both, 0.4).unwrap();
        assert_eq!(found.good_answer, "Fresh.");
    }

    #[test]
    fn a_stale_correction_is_marked_differently_in_the_prompt() {
        let mut stale = correction("q", "The answer.");
        stale.stale = true;

        assert!(prompt_block(&correction("q", "The answer.")).contains("authoritative"));
        assert!(prompt_block(&stale).contains("has changed"));
        assert!(!prompt_block(&stale).contains("authoritative"));
    }

    #[test]
    fn similarity_is_symmetric_and_bounded() {
        assert_eq!(similarity("a b c", "a b c"), 1.0);
        assert_eq!(similarity("a b", "c d"), 0.0);
        assert_eq!(similarity("", "a"), 0.0);
        assert_eq!(similarity("a b", "b a"), similarity("b a", "a b"));
    }
}
