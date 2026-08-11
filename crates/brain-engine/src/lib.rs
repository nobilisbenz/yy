//! Retrieval: seed → expand → rank → pack.
//!
//! The retrieval half of `PLAN.md` §2.3, and the part the project's own framing calls the
//! brain — the LLM is only the narrator. Four stages:
//!
//! 1. **Seed.** FTS5 BM25 over the vault, `search.lexical_limit` hits. Fast, and already
//!    good enough to be useful with no model at all.
//! 2. **Expand.** Walk `relations` outward from the top seeds, both directions, over the
//!    graph snapshot the writer thread precomputed. Backlinks matter as much as forward
//!    links: "what points at this" is usually the more interesting question, and a
//!    hand-authored `contradicts::` edge is better signal than anything an extraction pass
//!    would produce.
//! 3. **Rank.** Reciprocal rank fusion over the lexical and graph orderings, then
//!    front-matter status and heading-match multipliers.
//! 4. **Pack.** Truncate to `search.fused_limit` and attach an explanation to each result.
//!
//! **Fuse ranks, not scores.** BM25 returns a negative log-odds-ish number and graph
//! proximity returns a decayed product of edge weights. They share no scale, and
//! normalising them into one would be inventing a relationship that does not exist. RRF
//! only asks each signal for an ordering, which is the thing both can honestly supply.

pub mod actions;

use std::collections::HashMap;

use brain_core::config::{Fusion, GraphSearch, Search, StatusWeight};
use brain_index::{Hit, Index, IndexError};
use yalive::graph::{Expansion, RelationType};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Index(#[from] IndexError),
}

type Result<T> = std::result::Result<T, EngineError>;

/// Why a section is in the results, for the line under the source badge (`PLAN.md` §6.5).
///
/// This is the debugger for graph retrieval: when the wrong section comes back you can see
/// whether the seed was wrong or the expansion was, which is otherwise guesswork.
#[derive(Debug, Clone, Default)]
pub struct Explain {
    /// Position in the BM25 ordering, if it matched lexically at all.
    pub lexical_rank: Option<usize>,
    /// Position in the graph-proximity ordering, if expansion reached it.
    pub graph_rank: Option<usize>,
    /// 0 for a lexical seed.
    pub hops: usize,
    /// The edge that first reached it.
    pub via: Option<RelationType>,
    /// Whether that edge was followed backwards — i.e. this is a backlink.
    pub backwards: bool,
    pub heading_match: bool,
    pub status_weight: f32,
}

impl Explain {
    /// One line, in the vocabulary the dock shows.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.heading_match {
            parts.push("matched heading".to_string());
        } else if self.lexical_rank.is_some() {
            parts.push("matched text".to_string());
        }
        if let Some(relation) = self.via {
            let direction = if self.backwards { "back to" } else { "to" };
            let hops = self.hops;
            let plural = if hops == 1 { "hop" } else { "hops" };
            parts.push(format!(
                "{hops} {plural} {direction} {}",
                relation.as_str()
            ));
        }
        if self.status_weight < 1.0 {
            parts.push("superseded".to_string());
        }
        if parts.is_empty() {
            "matched".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

/// One ranked section.
#[derive(Debug, Clone)]
pub struct Ranked {
    pub hit: Hit,
    /// Fused score. Comparable within one result set and meaningless across queries.
    pub score: f32,
    pub explain: Explain,
}

/// Per-stage durations, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalTiming {
    pub seed_ms: u32,
    pub expand_ms: u32,
    pub rank_ms: u32,
    pub total_ms: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Retrieval {
    pub results: Vec<Ranked>,
    pub timing: RetrievalTiming,
    /// Seeds found before expansion, for logging and `brainctl bench`.
    pub seed_count: usize,
}

/// Run the pipeline.
///
/// Instrumented from the first commit rather than after a latency complaint: every stage
/// records its own duration under a span carrying the query, because retrofitting this
/// after the fact means guessing which stage got slow.
#[tracing::instrument(skip_all, fields(query_len = query.len()))]
pub async fn retrieve(index: &Index, config: &Search, query: &str) -> Result<Retrieval> {
    let started = std::time::Instant::now();

    // --- seed -------------------------------------------------------------------------
    let weights = bm25_weights(config);
    let seeds = index
        .search(query.to_string(), weights, config.lexical_limit)
        .await?;
    let seed_ms = started.elapsed().as_millis() as u32;
    let seed_count = seeds.len();

    if seeds.is_empty() {
        return Ok(Retrieval {
            results: Vec::new(),
            timing: RetrievalTiming {
                seed_ms,
                total_ms: started.elapsed().as_millis() as u32,
                ..Default::default()
            },
            seed_count: 0,
        });
    }

    // --- expand -----------------------------------------------------------------------
    let expand_started = std::time::Instant::now();
    let expansion = expand(index, config, &seeds).await?;
    let expand_ms = expand_started.elapsed().as_millis() as u32;

    // --- rank -------------------------------------------------------------------------
    let rank_started = std::time::Instant::now();
    let results = rank(config, seeds, expansion);
    let rank_ms = rank_started.elapsed().as_millis() as u32;

    let total_ms = started.elapsed().as_millis() as u32;
    tracing::debug!(
        seeds = seed_count,
        returned = results.len(),
        seed_ms,
        expand_ms,
        rank_ms,
        total_ms,
        "retrieved"
    );

    Ok(Retrieval {
        results,
        timing: RetrievalTiming {
            seed_ms,
            expand_ms,
            rank_ms,
            total_ms,
        },
        seed_count,
    })
}

fn bm25_weights(config: &Search) -> brain_index::fts::Bm25Weights {
    brain_index::fts::Bm25Weights {
        note_title: config.bm25_note_title,
        heading: config.bm25_heading,
        heading_path: config.bm25_heading_path,
        body: config.bm25_body,
        tags: config.bm25_tags,
    }
}

/// A section the graph reached, with everything needed to display it.
struct Expanded {
    hit: Hit,
    hops: usize,
    via: Option<RelationType>,
    backwards: bool,
}

/// Walk outward from the top seeds and resolve what we reach back to full rows.
async fn expand(index: &Index, config: &Search, seeds: &[Hit]) -> Result<Vec<Expanded>> {
    let settings: &GraphSearch = &config.graph;
    if !settings.enabled {
        return Ok(Vec::new());
    }

    let graph = index.graph();
    if graph.is_empty() {
        return Ok(Vec::new());
    }

    // Only the strongest seeds are worth expanding from. Expanding from all thirty pulls
    // in most of a well-linked vault and the ranking then has to undo it.
    let roots: Vec<(usize, f32)> = seeds
        .iter()
        .take(settings.seed_count)
        .enumerate()
        .filter_map(|(rank, hit)| {
            graph
                .index_of(&hit.section_uid)
                .map(|node| (node, 1.0 / (rank as f32 + 1.0)))
        })
        .collect();

    if roots.is_empty() {
        return Ok(Vec::new());
    }

    let reached = graph.expand(
        &roots,
        Expansion {
            max_hops: settings.max_hops,
            decay: settings.hop_decay,
            ..Default::default()
        },
    );

    // Seeds come back at hop 0; they are already in the lexical list, so only genuinely
    // new sections need resolving.
    let seen: std::collections::HashSet<&str> =
        seeds.iter().map(|hit| hit.section_uid.as_str()).collect();

    let mut wanted = Vec::new();
    let mut metadata = HashMap::new();
    for hit in &reached {
        if hit.hops == 0 || hit.score < settings.min_weight {
            continue;
        }
        let uid = graph.node(hit.index).uid.clone();
        if seen.contains(uid.as_str()) {
            continue;
        }
        metadata.insert(
            uid.clone(),
            (hit.hops, hit.via.map(|edge| edge.relation), hit.backwards),
        );
        wanted.push(uid);
    }

    if wanted.is_empty() {
        return Ok(Vec::new());
    }

    // Ordering is lost by the `IN` lookup, so reimpose the expansion's own order — the
    // graph rank is a ranking signal and returning them in SQLite's order would discard it.
    let order: HashMap<String, usize> = wanted
        .iter()
        .enumerate()
        .map(|(position, uid)| (uid.clone(), position))
        .collect();

    let rows = index
        .read(move |database| Ok(database.sections_by_uids(&wanted)?))
        .await?;

    let mut expanded: Vec<Expanded> = rows
        .into_iter()
        .map(Hit::from)
        .filter_map(|hit| {
            let (hops, via, backwards) = metadata.get(&hit.section_uid).copied()?;
            Some(Expanded {
                hit,
                hops,
                via,
                backwards,
            })
        })
        .collect();
    expanded.sort_by_key(|item| order.get(&item.hit.section_uid).copied().unwrap_or(usize::MAX));

    Ok(expanded)
}

/// Reciprocal rank fusion, then the multiplier post-filters.
fn rank(config: &Search, seeds: Vec<Hit>, expanded: Vec<Expanded>) -> Vec<Ranked> {
    let fusion: &Fusion = &config.fusion;
    let status: &StatusWeight = &config.status_weight;

    let mut ranked: Vec<Ranked> = Vec::with_capacity(seeds.len() + expanded.len());

    for (position, hit) in seeds.into_iter().enumerate() {
        ranked.push(Ranked {
            score: fusion.lexical_weight / (fusion.k + position as f32 + 1.0),
            explain: Explain {
                lexical_rank: Some(position),
                ..Default::default()
            },
            hit,
        });
    }

    for (position, item) in expanded.into_iter().enumerate() {
        ranked.push(Ranked {
            score: fusion.graph_weight / (fusion.k + position as f32 + 1.0),
            explain: Explain {
                graph_rank: Some(position),
                hops: item.hops,
                via: item.via,
                backwards: item.backwards,
                ..Default::default()
            },
            hit: item.hit,
        });
    }

    for entry in &mut ranked {
        // Front matter `status:` (spec §47). A workflow marked obsolete must not blend
        // into an answer with the note that replaced it.
        let weight = status.for_status(entry.hit.status.as_deref());
        entry.explain.status_weight = weight;
        entry.score *= weight;
    }

    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.truncate(config.fused_limit);
    ranked
}

/// Apply the heading-match boost for a query.
///
/// Separate from [`rank`] because it needs the query text, which the fusion stage
/// deliberately does not see — everything else there works on orderings alone.
pub fn boost_heading_matches(config: &Search, query: &str, results: &mut [Ranked]) {
    // Stopwords are excluded for the same reason they are excluded from the FTS
    // expression: `wireguard without a coordinator` must not mark every heading
    // containing the word "without" as a heading match, which reads as the boost being
    // meaningless — and, worse, actually reorders results on noise.
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|term| !yalive::search::is_stopword(term))
        .map(|term| term.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|term| term.len() > 2)
        .collect();
    if terms.is_empty() {
        return;
    }

    for entry in results.iter_mut() {
        let heading = entry.hit.heading.to_lowercase();
        if terms.iter().any(|term| heading.contains(term.as_str())) {
            entry.explain.heading_match = true;
            entry.score *= config.context_boost.heading_match;
        }
    }

    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// A ranked result carrying only the fields the action builder reads.
    fn stub(path: &str, line: usize) -> Ranked {
        Ranked {
            hit: Hit {
                section_uid: format!("{path}#root"),
                note_title: "Note".into(),
                heading: "Heading".into(),
                heading_path: "Heading".into(),
                body: String::new(),
                path: std::path::PathBuf::from(path),
                start_line: line,
                status: None,
            },
            score: 1.0,
            explain: Explain::default(),
        }
    }

    pub(crate) fn ranked_paths(paths: &[&str]) -> Vec<Ranked> {
        paths.iter().map(|path| stub(path, 1)).collect()
    }

    pub(crate) fn ranked_at_line(line: usize) -> Vec<Ranked> {
        vec![stub("/tmp/a.md", line)]
    }

    /// A vault where the lexically-best answer is the *wrong* one, and the note that
    /// corrects it is only reachable through the graph.
    fn vault() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("obs.md"),
            "---\nid: obs\ntitle: OBS\n---\n\
             # OBS {#root}\n\
             ## Old crop approach {#old}\n\
             Move the crop rectangle directly to the pointer on every frame.\n\
             ## Smoothing {#smooth}\n\
             contradicts:: [[obs#old]]\n\
             Apply exponential damping to the follow target instead.\n",
        )
        .unwrap();
        dir
    }

    async fn open(dir: &tempfile::TempDir) -> Index {
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();
        index
    }

    #[tokio::test]
    async fn a_lexical_hit_is_returned_with_its_rank_explained() {
        let dir = vault();
        let index = open(&dir).await;

        let found = retrieve(&index, &Search::default(), "crop rectangle").await.unwrap();
        assert!(!found.results.is_empty());
        let top = &found.results[0];
        assert_eq!(top.explain.lexical_rank, Some(0));
        assert_eq!(top.explain.hops, 0);
    }

    #[tokio::test]
    async fn expansion_reaches_the_section_that_contradicts_the_lexical_hit() {
        // The whole argument for graph retrieval. "crop rectangle" appears only in the old
        // approach; the note correcting it shares almost no vocabulary and BM25 alone
        // cannot connect them.
        let dir = vault();
        let index = open(&dir).await;

        let found = retrieve(&index, &Search::default(), "crop rectangle").await.unwrap();
        let smoothing = found
            .results
            .iter()
            .find(|entry| entry.hit.section_uid == "obs#smooth")
            .expect("the contradicting section was never reached");

        assert!(smoothing.explain.lexical_rank.is_none(), "it matched lexically after all");
        assert_eq!(smoothing.explain.hops, 1);
        assert_eq!(smoothing.explain.via, Some(RelationType::Contradicts));
        assert!(smoothing.explain.describe().contains("contradicts"));
    }

    #[tokio::test]
    async fn disabling_the_graph_leaves_only_lexical_results() {
        let dir = vault();
        let index = open(&dir).await;

        let mut config = Search::default();
        config.graph.enabled = false;
        let found = retrieve(&index, &config, "crop rectangle").await.unwrap();

        assert!(
            found.results.iter().all(|entry| entry.explain.hops == 0),
            "expansion ran with the graph disabled"
        );
    }

    #[tokio::test]
    async fn an_obsolete_note_is_demoted_below_an_equally_good_current_one() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("old.md"),
            "---\nid: old\ntitle: Old\nstatus: obsolete\n---\n# Root {#root}\nDeploytoken with rsync.\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("new.md"),
            "---\nid: new\ntitle: New\n---\n# Root {#root}\nDeploytoken with rsync.\n",
        )
        .unwrap();
        let index = open(&dir).await;

        let found = retrieve(&index, &Search::default(), "Deploytoken").await.unwrap();
        assert_eq!(found.results.len(), 2);
        assert_eq!(
            found.results[0].hit.section_uid, "new#root",
            "the obsolete note outranked the current one"
        );
        assert!(found.results[1].explain.describe().contains("superseded"));
    }

    #[tokio::test]
    async fn a_query_with_nothing_searchable_returns_nothing_rather_than_failing() {
        let dir = vault();
        let index = open(&dir).await;

        for query in ["", "   ", "--", "\"\""] {
            let found = retrieve(&index, &Search::default(), query).await.unwrap();
            assert!(found.results.is_empty(), "{query:?} returned results");
        }
    }

    #[tokio::test]
    async fn a_heading_match_outranks_a_body_match() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("n.md"),
            "---\nid: n\ntitle: N\n---\n\
             # Root {#root}\n\
             ## Buried mention {#body}\nA passing reference to Wireguardtoken here.\n\
             ## Wireguardtoken setup {#heading}\nThe actual instructions live here.\n",
        )
        .unwrap();
        let index = open(&dir).await;

        let config = Search::default();
        let mut found = retrieve(&index, &config, "Wireguardtoken").await.unwrap();
        boost_heading_matches(&config, "Wireguardtoken", &mut found.results);

        assert_eq!(found.results[0].hit.section_uid, "n#heading");
        assert!(found.results[0].explain.heading_match);
        assert!(found.results[0].explain.describe().contains("matched heading"));
    }
}
