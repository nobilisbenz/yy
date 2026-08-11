//! Configuration: `~/.config/brain/config.toml`.
//!
//! Every tunable lives here rather than in code, because Stage 7 settles the ranking
//! weights by benchmark sweep and a literal in a source file cannot be swept. The struct
//! shape mirrors `config/brain.example.toml`, including fields later stages read, so the
//! file does not churn as stages land.
//!
//! Two rules the loader enforces, both from the spec:
//!
//! - **Report every problem at once.** A config with three bad paths should take one edit
//!   to fix, not three runs. [`ConfigError::Invalid`] carries all of them.
//! - **Never index `$HOME` or `/`.** Spec §29. A source pointing at either is refused with
//!   a reason rather than quietly walking 400k files and heating the machine for an hour.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file at {}", .0.display())]
    Missing(PathBuf),
    #[error("could not read {}: {source}", .path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", .path.display())]
    Syntax {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// Every validation failure found, not just the first.
    #[error("{}", render(.path.as_deref(), .problems))]
    Invalid {
        path: Option<PathBuf>,
        problems: Vec<String>,
    },
    #[error("could not locate a config directory")]
    NoConfigDir,
}

fn render(path: Option<&Path>, problems: &[String]) -> String {
    let mut out = match path {
        Some(path) => format!("{} is not usable:", path.display()),
        None => "the configuration is not usable:".to_string(),
    };
    for problem in problems {
        let _ = write!(out, "\n  - {problem}");
    }
    out
}

type Result<T> = std::result::Result<T, ConfigError>;

// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub dock: Dock,
    pub logging: Logging,
    pub search: Search,
    pub indexing: Indexing,
    pub sources: Vec<Source>,
    pub openers: Openers,
    pub llm: Llm,
    pub answers: Answers,
    pub context: Context,
    pub retrieval: Retrieval,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dock {
    pub anchor: String,
    pub margin_top: u32,
    pub margin_side: u32,
    pub width: u32,
    pub max_height: u32,
    pub restore_previous_focus: bool,
    pub scale: f32,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            anchor: "top-right".into(),
            margin_top: 8,
            margin_side: 22,
            width: 560,
            max_height: 520,
            restore_previous_focus: true,
            scale: 1.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: String,
    /// Never log note contents by default (spec §50).
    pub log_queries: bool,
    pub log_source_paths: bool,
    pub debug_ui: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            level: "info".into(),
            log_queries: false,
            log_source_paths: true,
            debug_ui: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    /// How many BM25 hits seed the pipeline before expansion.
    pub lexical_limit: usize,
    /// How many survive fusion and ranking.
    pub fused_limit: usize,
    /// How many are packed into the prompt (Stage 2).
    pub context_sections: usize,

    pub bm25_note_title: f64,
    pub bm25_heading: f64,
    pub bm25_heading_path: f64,
    pub bm25_body: f64,
    pub bm25_tags: f64,

    /// Fused score the top result must reach before the model is called at all.
    ///
    /// This is the no-answer gate (spec §45), and it is deliberately *not* the model's
    /// judgement: asking a 1.7B model whether its context answers the question gets "yes"
    /// far too often. Kept permissive until the Stage 7 benchmark tunes it — a wrong "I
    /// don't know" is more annoying than a weak answer with a visible source under it.
    pub min_confidence: f32,

    pub status_weight: StatusWeight,
    pub context_boost: ContextBoost,
    pub fusion: Fusion,
    pub graph: GraphSearch,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            lexical_limit: 30,
            fused_limit: 20,
            context_sections: 5,
            // Heading matches carry most of the retrieval signal in a notes corpus.
            bm25_note_title: 3.0,
            bm25_heading: 8.0,
            bm25_heading_path: 4.0,
            bm25_body: 1.0,
            bm25_tags: 2.0,
            // RRF scores sit around 1/(k+rank); with k = 60 a top hit is ~0.016. This
            // floor therefore only rejects a result set that is empty or badly demoted by
            // status weighting, which is the intent until the benchmark says otherwise.
            min_confidence: 0.001,
            status_weight: StatusWeight::default(),
            context_boost: ContextBoost::default(),
            fusion: Fusion::default(),
            graph: GraphSearch::default(),
        }
    }
}

/// Ranking multipliers for front-matter `status:` (spec §47).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusWeight {
    pub current: f32,
    pub draft: f32,
    pub archived: f32,
    pub obsolete: f32,
}

impl Default for StatusWeight {
    fn default() -> Self {
        Self {
            current: 1.0,
            draft: 0.9,
            archived: 0.4,
            // Strongly penalise superseded workflows so old and new do not blend into one
            // answer — the failure mode a five-year-old vault has.
            obsolete: 0.25,
        }
    }
}

impl StatusWeight {
    pub fn for_status(&self, status: Option<&str>) -> f32 {
        match status.unwrap_or("current") {
            "draft" => self.draft,
            "archived" => self.archived,
            "obsolete" => self.obsolete,
            _ => self.current,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextBoost {
    pub active_app: f32,
    pub current_project: f32,
    pub heading_match: f32,
    pub recent_source: f32,
}

impl Default for ContextBoost {
    fn default() -> Self {
        Self {
            active_app: 1.25,
            current_project: 1.35,
            heading_match: 1.30,
            recent_source: 1.10,
        }
    }
}

/// Reciprocal rank fusion. Combines **ranks, not scores** — BM25 and graph proximity are
/// not on a common scale and normalising them is guesswork.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fusion {
    pub k: f32,
    pub lexical_weight: f32,
    pub semantic_weight: f32,
    pub graph_weight: f32,
}

impl Default for Fusion {
    fn default() -> Self {
        Self {
            k: 60.0,
            lexical_weight: 1.0,
            semantic_weight: 1.0,
            graph_weight: 1.0,
        }
    }
}

/// Graph expansion (`PLAN.md` §2.3 step 2).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphSearch {
    /// Off until the Phase D benchmark says it beats lexical-only.
    pub enabled: bool,
    pub max_hops: usize,
    /// How many top lexical hits are used as expansion seeds.
    pub seed_count: usize,
    pub hop_decay: f32,
    pub min_weight: f32,
}

impl Default for GraphSearch {
    fn default() -> Self {
        Self {
            enabled: true,
            max_hops: 2,
            seed_count: 5,
            hop_decay: 0.6,
            min_weight: 0.1,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Indexing {
    pub max_section_tokens: usize,
    pub subchunk_target_tokens: usize,
    pub subchunk_overlap_tokens: usize,
    /// Editors save in bursts; nvim's atomic save is a rename pair.
    pub debounce_ms: u64,
}

impl Default for Indexing {
    fn default() -> Self {
        Self {
            max_section_tokens: 700,
            subchunk_target_tokens: 450,
            subchunk_overlap_tokens: 60,
            debounce_ms: 400,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// A vault is read through `yalive` and participates in graph and review-state ranking.
    /// Anything else is a plain lexical source (`PLAN.md` §2.2).
    #[serde(default = "default_true")]
    pub vault: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: PathBuf::new(),
            include: default_include(),
            exclude: Vec::new(),
            vault: true,
        }
    }
}

fn default_include() -> Vec<String> {
    vec!["**/*.md".into()]
}

fn default_true() -> bool {
    true
}

impl Source {
    /// Compile the include/exclude globs.
    ///
    /// Separate from validation so the walker builds them once rather than per file.
    pub fn matcher(&self) -> std::result::Result<SourceMatcher, globset::Error> {
        Ok(SourceMatcher {
            include: build_globset(&self.include)?,
            exclude: build_globset(&self.exclude)?,
        })
    }
}

fn build_globset(patterns: &[String]) -> std::result::Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    builder.build()
}

pub struct SourceMatcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl SourceMatcher {
    /// Exclude wins over include, which is what makes `**/*.md` plus `**/.notes/**` mean
    /// what a reader expects.
    pub fn accepts(&self, path: &Path) -> bool {
        if self.exclude.is_match(path) {
            return false;
        }
        self.include.is_empty() || self.include.is_match(path)
    }

    /// Should a walker descend into this directory?
    ///
    /// Only the exclude set applies: a directory almost never matches an include pattern
    /// like `**/*.md`, so testing includes here would prune everything. Pruning matters —
    /// on a source pointed at a project tree, refusing to descend into `target/` is the
    /// difference between instant and a minute.
    pub fn accepts_directory(&self, path: &Path) -> bool {
        !self.exclude.is_match(path)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Openers {
    pub markdown: Vec<String>,
    pub text: Vec<String>,
    pub directory: Vec<String>,
    pub url: Vec<String>,
    pub video: Vec<String>,
}

impl Default for Openers {
    fn default() -> Self {
        Self {
            markdown: shell(&["ghostty", "-e", "nvim", "+{line}", "{path}"]),
            text: shell(&["ghostty", "-e", "nvim", "+{line}", "{path}"]),
            directory: shell(&["xdg-open", "{path}"]),
            url: shell(&["xdg-open", "{url}"]),
            video: shell(&["xdg-open", "{url}"]),
        }
    }
}

fn shell(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|part| (*part).to_string()).collect()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub backend: String,
    pub profile: String,
    pub host: String,
    pub port: u16,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub keep_loaded: bool,
    #[serde(default)]
    pub profiles: std::collections::BTreeMap<String, LlmProfile>,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            backend: "llama-server".into(),
            profile: "fast".into(),
            host: "127.0.0.1".into(),
            port: 8177,
            context_tokens: 4096,
            // PLAN.md §3.1: 350 tokens at the measured 168 t/s is 2.1 s of the budget.
            max_output_tokens: 200,
            temperature: 0.15,
            top_p: 0.85,
            keep_loaded: true,
            profiles: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmProfile {
    pub model: PathBuf,
    #[serde(default)]
    pub draft_model: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Answers {
    /// Do not let the model improvise from pretraining. A confident "not in your files" is
    /// a feature (spec §45).
    pub general_knowledge_fallback: bool,
    pub show_source: bool,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            general_knowledge_fallback: false,
            show_source: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Context {
    pub enabled: bool,
    #[serde(default)]
    pub aliases: std::collections::BTreeMap<String, Vec<String>>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            enabled: true,
            aliases: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retrieval {
    pub reranker: Reranker,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reranker {
    /// Costs 50–150 ms. On only if the Stage 7 benchmark says it earns that.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------------------

impl Config {
    /// The default config path, `$XDG_CONFIG_HOME/brain/config.toml`.
    pub fn default_path() -> Result<PathBuf> {
        use etcetera::BaseStrategy as _;
        let strategy = etcetera::choose_base_strategy().map_err(|_| ConfigError::NoConfigDir)?;
        Ok(strategy.config_dir().join("brain/config.toml"))
    }

    /// Load and validate the config at the default path.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::default_path()?)
    }

    /// Load and validate a specific file.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(ConfigError::Missing(path.to_path_buf()));
        }
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Config = toml::from_str(&text).map_err(|source| ConfigError::Syntax {
            path: path.to_path_buf(),
            source,
        })?;

        config.normalise();
        config.validate().map_err(|problems| ConfigError::Invalid {
            path: Some(path.to_path_buf()),
            problems,
        })?;
        Ok(config)
    }

    /// Expand `~` in every path the file can carry.
    ///
    /// Done ourselves because no crate handles `~otheruser` correctly and we do not need
    /// that: only a leading `~` or `~/` is expanded, and anything else is left alone.
    fn normalise(&mut self) {
        for source in &mut self.sources {
            source.path = expand_tilde(&source.path);
        }
        for profile in self.llm.profiles.values_mut() {
            profile.model = expand_tilde(&profile.model);
            profile.draft_model = profile.draft_model.as_deref().map(expand_tilde);
        }
    }

    /// Every problem at once, so one edit fixes the file.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut problems = Vec::new();

        if self.sources.is_empty() {
            problems.push("no [[sources]] configured; there is nothing to index".into());
        }

        let home = home_dir();
        let mut names: Vec<&str> = Vec::new();

        for (index, source) in self.sources.iter().enumerate() {
            let label = if source.name.is_empty() {
                format!("sources[{index}]")
            } else {
                format!("source {:?}", source.name)
            };

            if source.name.is_empty() {
                problems.push(format!("{label}: needs a name"));
            } else if names.contains(&source.name.as_str()) {
                problems.push(format!("{label}: duplicate name"));
            } else {
                names.push(&source.name);
            }

            // Spec §29. Refusing beats walking 400k files and finding out later.
            if source.path == Path::new("/") {
                problems.push(format!("{label}: refusing to index the filesystem root"));
            } else if home.as_deref().is_some_and(|home| source.path == home) {
                problems.push(format!(
                    "{label}: refusing to index $HOME wholesale; point at a subdirectory"
                ));
            } else if !source.path.exists() {
                problems.push(format!("{label}: {} does not exist", source.path.display()));
            } else if !source.path.is_dir() {
                problems.push(format!(
                    "{label}: {} is not a directory",
                    source.path.display()
                ));
            }

            for pattern in source.include.iter().chain(&source.exclude) {
                if let Err(error) = Glob::new(pattern) {
                    problems.push(format!("{label}: bad glob {pattern:?}: {error}"));
                }
            }
        }

        for (name, argv) in [
            ("openers.markdown", &self.openers.markdown),
            ("openers.text", &self.openers.text),
            ("openers.directory", &self.openers.directory),
            ("openers.url", &self.openers.url),
            ("openers.video", &self.openers.video),
        ] {
            if argv.is_empty() {
                problems.push(format!("{name}: needs at least a program name"));
            }
        }

        if !self.llm.profiles.contains_key(&self.llm.profile) && !self.llm.profiles.is_empty() {
            problems.push(format!(
                "llm.profile = {:?} has no matching [llm.profiles.{}]",
                self.llm.profile, self.llm.profile
            ));
        }

        if self.search.lexical_limit == 0 {
            problems.push("search.lexical_limit must be at least 1".into());
        }
        if self.search.context_sections > self.search.fused_limit {
            problems.push(format!(
                "search.context_sections ({}) exceeds search.fused_limit ({}), so the extra \
                 sections can never be reached",
                self.search.context_sections, self.search.fused_limit
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// The sources read through `yalive` as vaults.
    pub fn vaults(&self) -> impl Iterator<Item = &Source> {
        self.sources.iter().filter(|source| source.vault)
    }
}

/// Expand a leading `~` against `$HOME`.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match home_dir() {
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_shipped_example_config_loads() {
        // The example file is the documentation for this struct. If it stops parsing, the
        // documentation is wrong, and `brainctl init` copies a file that does not work.
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/brain.example.toml");
        let text = fs::read_to_string(&example).expect("the example config is missing");
        let config: Config = toml::from_str(&text).expect("the example config does not parse");

        assert_eq!(config.dock.width, 560);
        assert!(!config.sources.is_empty(), "the example configures a source");
        assert!(!config.answers.general_knowledge_fallback);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let dir = tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"
            [[sources]]
            name = "a"
            path = "/definitely/not/here"
            include = ["**/*.md"]

            [[sources]]
            name = "a"
            path = "/also/not/here"
            include = ["["]
            "#,
        );

        let Err(ConfigError::Invalid { problems, .. }) = Config::load_from(&path) else {
            panic!("a config with four problems in it loaded");
        };
        // Duplicate name, two missing paths, one uncompilable glob. Reporting only the
        // first would mean four edit-run cycles to fix one file.
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(problems.iter().any(|p| p.contains("duplicate name")));
        assert!(problems.iter().any(|p| p.contains("bad glob")));
    }

    #[test]
    fn indexing_home_or_the_root_is_refused() {
        let dir = tempdir().unwrap();
        let home = std::env::var("HOME").unwrap();
        let path = write(
            dir.path(),
            &format!(
                r#"
                [[sources]]
                name = "home"
                path = "{home}"

                [[sources]]
                name = "root"
                path = "/"
                "#
            ),
        );

        let Err(ConfigError::Invalid { problems, .. }) = Config::load_from(&path) else {
            panic!("indexing $HOME and / was allowed");
        };
        assert!(problems.iter().any(|p| p.contains("$HOME")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("root")), "{problems:?}");
    }

    #[test]
    fn a_tilde_path_is_expanded_before_it_is_checked() {
        let dir = tempdir().unwrap();
        // `~` must expand, or this source reads as a literal directory named "~" and the
        // error message sends the reader looking in the wrong place.
        let path = write(
            dir.path(),
            r#"
            [[sources]]
            name = "notes"
            path = "~/definitely-not-a-real-directory"
            "#,
        );
        let Err(ConfigError::Invalid { problems, .. }) = Config::load_from(&path) else {
            panic!("a missing source directory was accepted");
        };
        let home = std::env::var("HOME").unwrap();
        assert!(problems[0].contains(&home), "{problems:?}");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_silently_ignored() {
        // A typo in a weight name would otherwise leave the default in place and the user
        // convinced they had tuned something.
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "[search]\nbm25_headding = 9.0\n");
        assert!(matches!(
            Config::load_from(&path),
            Err(ConfigError::Syntax { .. })
        ));
    }

    #[test]
    fn a_missing_file_says_which_file() {
        let path = Path::new("/nonexistent/brain/config.toml");
        let error = Config::load_from(path).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/brain/config.toml"));
    }

    #[test]
    fn defaults_alone_are_valid_apart_from_having_no_sources() {
        let problems = Config::default().validate().unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no [[sources]]"));
    }

    #[test]
    fn exclude_beats_include_in_the_matcher() {
        let source = Source {
            name: "notes".into(),
            path: PathBuf::from("/tmp"),
            include: vec!["**/*.md".into()],
            exclude: vec!["**/.notes/**".into()],
            vault: true,
        };
        let matcher = source.matcher().unwrap();
        assert!(matcher.accepts(Path::new("/tmp/a.md")));
        assert!(!matcher.accepts(Path::new("/tmp/.notes/b.md")));
        assert!(!matcher.accepts(Path::new("/tmp/a.rs")));
    }
}
