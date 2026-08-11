//! The retrieval regression gate.
//!
//! Stage 7's definition of done: a parser or ranking regression should fail the test suite,
//! not be noticed weeks later when an answer looks wrong.
//!
//! This runs against the **frozen fixture vault**, never the live one — a test whose result
//! depends on notes the author edited yesterday is not a regression test. The floor is set
//! below the current score on purpose: it is a tripwire for "something broke", not a target
//! to optimise, and a floor equal to the current score would fail on any harmless
//! reordering.

use std::path::PathBuf;

use brain_bench::{load_questions, run};
use brain_core::Config;

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// Recall@3 must not fall below this.
///
/// Currently 1.00 on six questions. A drop to 0.83 is one question breaking, which is worth
/// failing over; the gap leaves room for a legitimate reordering that does not lose one.
const RECALL_AT_3_FLOOR: f64 = 0.80;

/// MRR is the second gate: recall can hold while everything slides from rank 1 to rank 3,
/// and the dock only shows one primary source.
const MRR_FLOOR: f64 = 0.75;

#[tokio::test]
async fn the_fixture_vault_still_retrieves_its_known_answers() {
    let vault = fixture("tests/fixtures/vault");
    let questions = load_questions(&fixture("benchmarks/fixture.yaml")).expect("question set");
    assert!(!questions.is_empty());

    // Defaults, not the user's config: this must give the same answer on any machine.
    let report = run(&vault, &Config::default(), &questions)
        .await
        .expect("benchmark run");

    let failing: Vec<&str> = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.rank.is_none())
        .map(|outcome| outcome.question.as_str())
        .collect();

    assert!(
        report.recall_at_3 >= RECALL_AT_3_FLOOR,
        "Recall@3 {:.2} is below the floor of {RECALL_AT_3_FLOOR:.2}\n{}\nnever matched: {failing:#?}",
        report.recall_at_3,
        report.render("regression")
    );

    assert!(
        report.mrr >= MRR_FLOOR,
        "MRR {:.2} is below the floor of {MRR_FLOOR:.2} — results are still found but are \
         ranking lower\n{}",
        report.mrr,
        report.render("regression")
    );
}

/// The contradicting section is reachable even though it shares no vocabulary with the
/// question.
///
/// Called out separately from the aggregate because it is the single clearest argument for
/// graph retrieval existing at all: "how do I stop the OBS crop from jittering?" contains
/// the word *jitter*, which appears only in the section being **corrected**, not in the fix.
/// Lexical search alone cannot connect them.
#[tokio::test]
async fn graph_retrieval_still_beats_lexical_alone_on_the_case_it_exists_for() {
    let vault = fixture("tests/fixtures/vault");
    let questions = load_questions(&fixture("benchmarks/fixture.yaml")).expect("question set");

    let with_graph = run(&vault, &Config::default(), &questions)
        .await
        .expect("benchmark run");

    let mut lexical_only = Config::default();
    lexical_only.search.graph.enabled = false;
    let without_graph = run(&vault, &lexical_only, &questions)
        .await
        .expect("benchmark run");

    assert!(
        with_graph.mrr >= without_graph.mrr,
        "graph expansion made retrieval worse: MRR {:.2} with, {:.2} without",
        with_graph.mrr,
        without_graph.mrr
    );
}
