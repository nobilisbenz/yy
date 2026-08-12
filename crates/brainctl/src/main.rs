//! `brainctl` — one request, one answer, exit.
//!
//! This is what i3 binds to. It must start fast and it must fail loudly: a
//! silent no-op when the daemon is down would look exactly like a broken
//! keybinding, and you would go looking in the wrong place.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use brain_proto::{ClientConnection, ClientRequest, ProtoError, ServerEvent, socket_path};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "brainctl", version, about = "Brain Dock control client")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show the dock if hidden, hide it if shown. Bound to $mod+a in i3.
    Toggle,
    Show,
    Hide,
    /// Ask a question and stream the answer to stdout.
    Ask {
        /// The question. Quote it.
        query: Vec<String>,
        /// Retrieve and print sources without generating an answer.
        #[arg(long)]
        no_llm: bool,
        /// Save this as the correct answer to the question.
        ///
        /// Asks, then corrects — the same two steps the dock's Ctrl+E performs, scripted.
        #[arg(long)]
        correct: Option<String>,
        /// Ignore what was focused when ranking.
        ///
        /// When context ranking misfires, the only way to confirm that is what happened is
        /// to turn it off and ask the same thing again (spec §18).
        #[arg(long)]
        no_context: bool,
    },
    /// Print daemon diagnostics (spec §38).
    Status,
    /// Walk the configured sources and bring the index up to date.
    Reindex,
    PauseIndexing,
    ResumeIndexing,
    /// List the configured sources and what they will index.
    Sources,
    /// Time retrieval over a set of queries and report p50/p99.
    ///
    /// The latency targets in the plan are unfalsifiable without this — on a five-row
    /// fixture vault they pass regardless of what the code does.
    Bench {
        /// Queries to time. Defaults to a spread of shapes if none are given.
        queries: Vec<String>,
        /// Repetitions per query. The first run of each is discarded as a warm-up.
        #[arg(long, default_value_t = 20)]
        runs: usize,
        /// Also generate, and report TTFT and tokens/sec.
        ///
        /// Off by default because generation is ~1000x slower than retrieval, so the two
        /// want very different run counts to say anything useful.
        #[arg(long)]
        generate: bool,
    },
    /// Turn rated answers into a benchmark question set.
    ///
    /// The alternative is sitting down and inventing 30–50 questions with known-correct
    /// sections. These come from what was actually asked, and the label is the set of
    /// sections that produced an answer marked good — which is strictly better data and
    /// arrives as a side effect of using the tool (`PLAN.md` §6.3).
    BenchExport {
        /// Where to write the YAML. Defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Refuse to write fewer than this many questions.
        ///
        /// A benchmark of six real questions is worse than no benchmark, because it looks
        /// like evidence and moves on noise.
        #[arg(long, default_value_t = 20)]
        min_questions: usize,
    },
    /// Review stored corrections.
    #[command(subcommand)]
    Corrections(CorrectionCommand),
    /// Check the environment: config, socket, compositor, openers.
    Doctor,
    /// The graph panel inside the dock.
    #[command(subcommand)]
    Graph(GraphCommand),
}

#[derive(Subcommand, Debug)]
enum CorrectionCommand {
    /// List corrections, newest first.
    List {
        /// Only those whose source has been rewritten since they were confirmed.
        #[arg(long)]
        stale: bool,
    },
    /// Forget a correction.
    Delete { id: i64 },
}

#[derive(Subcommand, Debug)]
enum GraphCommand {
    /// Show the graph panel if hidden, hide it if shown.
    ///
    /// The daemon owns the flag, exactly as it owns dock visibility — which is what keeps
    /// this binary stateless and lets an i3 binding be a one-liner.
    Toggle,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("brainctl: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(args.command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("brainctl: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<()> {
    // Handled without the daemon: both read the same config file the daemon does, which is
    // what lets `doctor` diagnose a config the daemon refused to start on.
    match command {
        Command::Doctor => return doctor().await,
        Command::Sources => return print_sources(),
        Command::BenchExport { out, min_questions } => {
            return export_benchmark(out.as_deref(), min_questions);
        }
        Command::Corrections(ref command) => return manage_corrections(command),
        _ => {}
    }

    let mut connection = connect().await?;

    match command {
        Command::Toggle => connection.send(&ClientRequest::Toggle).await?,
        Command::Show => connection.send(&ClientRequest::Show).await?,
        Command::Hide => connection.send(&ClientRequest::Hide).await?,
        Command::Graph(GraphCommand::Toggle) => {
            connection.send(&ClientRequest::ToggleGraph).await?
        }
        Command::PauseIndexing => connection.send(&ClientRequest::PauseIndexing).await?,
        Command::ResumeIndexing => connection.send(&ClientRequest::ResumeIndexing).await?,

        Command::Status => {
            connection.send(&ClientRequest::Status).await?;
            return print_status(&mut connection).await;
        }
        Command::Reindex => {
            connection.send(&ClientRequest::Reindex).await?;
            // The daemon answers a reindex with a fresh `Status`, not a `Complete` —
            // `Complete` belongs to queries. Printing the resulting counts is also the
            // only way to see that the walk actually found anything.
            return print_status(&mut connection).await;
        }
        Command::Bench {
            queries,
            runs,
            generate,
        } => {
            return bench(&mut connection, queries, runs, generate).await;
        }
        Command::Ask {
            query,
            no_llm,
            no_context,
            correct,
        } => {
            let text = query.join(" ");
            anyhow::ensure!(!text.trim().is_empty(), "no question given");
            let id = Uuid::new_v4();
            connection
                .send(&ClientRequest::Query {
                    id,
                    text,
                    // An empty context means "use the last summon's"; the daemon cannot
                    // tell that from "I deliberately want none", so say which.
                    context: if no_context {
                        suppressed_context()
                    } else {
                        Default::default()
                    },
                    retrieval_only: no_llm || correct.is_some(),
                })
                .await?;

            if let Some(answer) = correct {
                // Retrieval has to finish first: the correction records which sections the
                // answer was based on, and the daemon only knows those once it has run.
                stream_answer(&mut connection).await?;
                connection
                    .send(&ClientRequest::SaveCorrection { id, answer })
                    .await?;
                // Sent, not confirmed: the daemon saves asynchronously and this process
                // exits before that lands. Claiming otherwise would be a small lie that
                // shows up as `corrections list` looking empty a moment later.
                println!("correction sent");
                return Ok(());
            }
            return stream_answer(&mut connection).await;
        }
        Command::Doctor
        | Command::Sources
        | Command::BenchExport { .. }
        | Command::Corrections(_) => unreachable!("handled above"),
    }

    // Fire-and-forget commands still need the write to reach the kernel before
    // the process exits, which `send` has already ensured. Nothing to await.
    Ok(())
}

async fn connect() -> Result<ClientConnection> {
    let path = socket_path().context("resolving the control socket path")?;
    ClientConnection::connect(&path).await.map_err(|err| match err {
        ProtoError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            anyhow::anyhow!(
                "brain-daemon is not running (no socket at {})\n\
                 Start it with `brain-daemon`, or check `systemctl --user status brain-daemon`.",
                path.display()
            )
        }
        other => anyhow::Error::new(other).context("connecting to brain-daemon"),
    })
}

async fn print_status(connection: &mut ClientConnection) -> Result<()> {
    loop {
        match connection.recv_expected().await? {
            ServerEvent::Status(report) => {
                let field = |name: &str, value: String| println!("{name:<20}{value}");
                field("daemon", format!("running (v{})", report.daemon_version));
                field("uptime", format!("{}s", report.uptime_seconds));
                field(
                    "llm",
                    report.llm_model.clone().unwrap_or_else(|| "-".into()),
                );
                field(
                    "backend",
                    report.llm_backend.clone().unwrap_or_else(|| "-".into()),
                );
                field("model state", report.llm_state.clone());
                field(
                    "context",
                    report
                        .llm_context_tokens
                        .map_or_else(|| "-".into(), |n| n.to_string()),
                );
                field("indexed documents", report.indexed_documents.to_string());
                field("indexed sections", report.indexed_sections.to_string());
                field("embedding queue", report.embedding_queue.to_string());
                field("index generation", report.index_generation.to_string());
                field("indexing", if report.indexing_paused { "paused".into() } else { "running".to_string() });
                field("dock", if report.ui_connected { "connected".into() } else { "not connected".to_string() });
                field("dock visible", report.dock_visible.to_string());
                field(
                    "answers recorded",
                    format!(
                        "{} ({} good, {} bad)",
                        report.answers_recorded,
                        report.answers_rated_good,
                        report.answers_rated_bad
                    ),
                );
                if report.stale_corrections > 0 {
                    field(
                        "stale corrections",
                        format!(
                            "{} (brainctl corrections list --stale)",
                            report.stale_corrections
                        ),
                    );
                }
                if let Some(t) = report.last_query {
                    field(
                        "last query",
                        format!("{} ms retrieval / {} ms TTFT", t.retrieval_ms, t.ttft_ms),
                    );
                }
                return Ok(());
            }
            ServerEvent::Error { message, .. } => anyhow::bail!("{message}"),
            _ => continue,
        }
    }
}

async fn stream_answer(connection: &mut ClientConnection) -> Result<()> {
    use std::io::Write;

    let mut saw_output = false;
    while let Some(event) = connection.recv().await {
        match event? {
            ServerEvent::Sources { items, .. } => {
                for (i, source) in items.iter().enumerate() {
                    println!(
                        "[{}] {}:{} · {}",
                        i + 1,
                        source.path.display(),
                        source.start_line,
                        source.heading_path
                    );
                    // Why this result, not just which. The debugger for graph retrieval:
                    // when the wrong section comes back, this says whether the seed or the
                    // expansion was at fault.
                    if !source.explain.is_empty() {
                        println!("    {}", source.explain);
                    }
                }
                if !items.is_empty() {
                    println!();
                }
                saw_output = true;
            }
            // What `Alt+1..9` would do. Printed because it is the only way to check the
            // action pipeline without a running dock, and because a disabled button is a
            // note that needs fixing.
            ServerEvent::Actions { items, .. } => {
                for (index, action) in items.iter().enumerate() {
                    let state = if action.enabled { "" } else { "  (unavailable)" };
                    println!(
                        "Alt+{}  {:<20} {:?}  {}{state}",
                        index + 1,
                        action.label,
                        action.kind,
                        action.detail
                    );
                }
                if !items.is_empty() {
                    println!();
                }
                saw_output = true;
            }
            ServerEvent::Token { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                saw_output = true;
            }
            ServerEvent::NoAnswer { closest, .. } => {
                println!("No reliable answer in the indexed files.");
                for source in &closest {
                    println!("  {}  {}", source.path.display(), source.heading_path);
                }
                return Ok(());
            }
            ServerEvent::Complete { timing, .. } => {
                if saw_output {
                    println!();
                }
                eprintln!(
                    "({} ms retrieval, {} ms TTFT, {} ms total)",
                    timing.retrieval_ms, timing.ttft_ms, timing.total_ms
                );
                return Ok(());
            }
            ServerEvent::Error { message, .. } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    anyhow::bail!("brain-daemon closed the connection before finishing")
}

/// Queries used when `bench` is given none.
///
/// Chosen for *shape* rather than for a particular vault: a natural question, a bare
/// keyword pair, an identifier that only survives with `tokenchars`, and a query full of
/// punctuation that would crash an unescaped `MATCH`.
const DEFAULT_BENCH_QUERIES: &[&str] = &[
    "how do I schedule a nightly backup?",
    "systemd timer",
    "calculate_pivot",
    "what's the -j flag for?",
];

/// Time retrieval and report percentiles.
///
/// Retrieval only: generation is not wired yet, and mixing the two would report a number
/// dominated by a stage that does not exist.
async fn bench(
    connection: &mut ClientConnection,
    queries: Vec<String>,
    runs: usize,
    generate: bool,
) -> Result<()> {
    anyhow::ensure!(runs > 1, "need at least 2 runs; the first is a warm-up");

    let queries = if queries.is_empty() {
        DEFAULT_BENCH_QUERIES
            .iter()
            .map(|q| (*q).to_string())
            .collect()
    } else {
        queries
    };

    if generate {
        println!(
            "{:<34} {:>6} {:>9} {:>9} {:>9} {:>8}",
            "query", "n", "p50 total", "p50 TTFT", "p99 TTFT", "tok/s"
        );
    } else {
        println!("{:<40} {:>7} {:>7} {:>7} {:>7}", "query", "n", "p50", "p99", "max");
    }

    let mut all = Vec::new();
    let mut all_ttft = Vec::new();
    let mut all_rates = Vec::new();

    for query in &queries {
        let mut samples = Vec::with_capacity(runs);
        let mut ttfts = Vec::with_capacity(runs);
        let mut rates = Vec::with_capacity(runs);
        let mut sources = 0;

        for run in 0..runs {
            let started = std::time::Instant::now();
            connection
                .send(&ClientRequest::Query {
                    id: Uuid::new_v4(),
                    text: query.clone(),
                    context: Default::default(),
                    retrieval_only: !generate,
                })
                .await?;

            let outcome = wait_for_complete(connection).await?;
            // Discard the first: it pays for the connection being cold and, on a fresh
            // daemon, for the page cache and an unprimed KV cache. Reporting it would
            // flatter or damn the run depending only on what ran before it.
            if run > 0 {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                if outcome.timing.ttft_ms > 0 {
                    ttfts.push(outcome.timing.ttft_ms as f64);
                }
                if outcome.timing.generation_ms > 0 && outcome.timing.output_tokens > 0 {
                    rates.push(
                        outcome.timing.output_tokens as f64
                            / (outcome.timing.generation_ms as f64 / 1000.0),
                    );
                }
            }
            sources = outcome.sources;
        }

        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap());

        if generate {
            let mean_rate = if rates.is_empty() {
                0.0
            } else {
                rates.iter().sum::<f64>() / rates.len() as f64
            };
            println!(
                "{:<34} {:>6} {:>7.0}ms {:>7.0}ms {:>7.0}ms {:>8.1}",
                truncate(query, 34),
                samples.len(),
                percentile(&samples, 0.50),
                percentile(&ttfts, 0.50),
                percentile(&ttfts, 0.99),
                mean_rate,
            );
            all_rates.extend(rates);
        } else {
            println!(
                "{:<40} {:>7} {:>6.1}ms {:>6.1}ms {:>6.1}ms   ({sources} sources)",
                truncate(query, 40),
                samples.len(),
                percentile(&samples, 0.50),
                percentile(&samples, 0.99),
                samples.last().copied().unwrap_or(0.0),
            );
        }
        all.extend(samples);
        all_ttft.extend(ttfts);
    }

    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    all_ttft.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!(
        "\noverall  p50 {:.1} ms   p99 {:.1} ms   over {} samples",
        percentile(&all, 0.50),
        percentile(&all, 0.99),
        all.len()
    );

    if generate {
        let mean_rate = if all_rates.is_empty() {
            0.0
        } else {
            all_rates.iter().sum::<f64>() / all_rates.len() as f64
        };
        println!(
            "TTFT     p50 {:.0} ms   p99 {:.0} ms          {:.1} tok/s generating",
            percentile(&all_ttft, 0.50),
            percentile(&all_ttft, 0.99),
            mean_rate
        );
        // The spec's target, stated so the measured number has something to mean.
        println!("target   TTFT under 500 ms warm");
    } else {
        println!("target   end-to-end query → UI under 100 ms");
    }
    Ok(())
}

/// What one benchmarked query produced.
struct Outcome {
    sources: usize,
    timing: brain_proto::TimingInfo,
}

/// Wait for a query to finish, returning what it produced.
async fn wait_for_complete(connection: &mut ClientConnection) -> Result<Outcome> {
    let mut sources = 0;
    while let Some(event) = connection.recv().await {
        match event? {
            ServerEvent::Sources { items, .. } => sources = items.len(),
            ServerEvent::Complete { timing, .. } => return Ok(Outcome { sources, timing }),
            ServerEvent::Error { message, .. } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    anyhow::bail!("brain-daemon closed the connection before finishing")
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let cut: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// What the config says will be indexed.
fn print_sources() -> Result<()> {
    let config = load_config()?;

    if config.sources.is_empty() {
        println!("no [[sources]] configured");
        return Ok(());
    }

    for source in &config.sources {
        let kind = if source.vault { "vault" } else { "lexical" };
        println!("{}  ({kind})", source.name);
        println!("  path     {}", source.path.display());
        println!("  include  {}", source.include.join(", "));
        if !source.exclude.is_empty() {
            println!("  exclude  {}", source.exclude.join(", "));
        }
        // Counting is what turns "I configured a source" into "the source has files in
        // it" — a glob that matches nothing looks identical to a working one otherwise.
        match count_matching(source) {
            Ok(count) => println!("  files    {count}"),
            Err(error) => println!("  files    unreadable: {error}"),
        }
        println!();
    }
    Ok(())
}

/// Files a source would index, walking with its own globs.
fn count_matching(source: &brain_core::Source) -> Result<usize> {
    let matcher = source.matcher()?;
    let mut count = 0;
    let mut stack = vec![source.path.clone()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Prune excluded directories rather than walking into them: on a source
                // pointed at a project tree, descending into `target/` is the difference
                // between instant and a minute.
                if matcher.accepts_directory(&path) {
                    stack.push(path);
                }
            } else if matcher.accepts(&path) {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// List or delete stored corrections.
///
/// A stale correction is one whose source note was rewritten after it was confirmed. It is
/// still applied — silently dropping an explicit user correction is worse than showing a
/// possibly-outdated one — so this is where you re-confirm or forget it.
fn manage_corrections(command: &CorrectionCommand) -> Result<()> {
    let path = brain_engine::store::Store::default_path().context("could not locate the store")?;
    let store = brain_engine::store::Store::open(&path)?;

    match command {
        CorrectionCommand::Delete { id } => {
            if store.delete_correction(*id)? {
                println!("deleted correction {id}");
            } else {
                println!("no correction with id {id}");
            }
        }
        CorrectionCommand::List { stale } => {
            let all = store.corrections()?;
            let shown: Vec<_> = all
                .iter()
                .filter(|correction| !*stale || correction.stale)
                .collect();

            if shown.is_empty() {
                println!("no corrections{}", if *stale { " are stale" } else { "" });
                return Ok(());
            }

            for correction in shown {
                let mark = if correction.stale { "  (stale)" } else { "" };
                println!("[{}]{mark} {}", correction.id, correction.question);
                println!("    {}", correction.good_answer);
                if !correction.sources.is_empty() {
                    let uids: Vec<&str> = correction
                        .sources
                        .iter()
                        .map(|(uid, _)| uid.as_str())
                        .collect();
                    println!("    based on: {}", uids.join(", "));
                }
                println!();
            }
        }
    }
    Ok(())
}

/// Write a benchmark question set from the answers marked good.
///
/// Only `good` rows become questions. A `bad` rating says the retrieved sections were the
/// wrong ones, so it is a record of a failure rather than a label — using it would assert
/// that the wrong answer is correct.
fn export_benchmark(out: Option<&std::path::Path>, min_questions: usize) -> Result<()> {
    let path = brain_engine::store::Store::default_path()
        .context("could not locate the store")?;
    let store = brain_engine::store::Store::open(&path)?;

    let rated = store.rated()?;
    let questions: Vec<brain_bench::Question> = rated
        .into_iter()
        .filter(|row| row.rating.is_some_and(|rating| rating > 0))
        .filter(|row| !row.section_uids.is_empty())
        .map(|row| brain_bench::Question {
            question: row.query,
            expected: row.section_uids,
            context: None,
            note: None,
        })
        .collect();

    anyhow::ensure!(
        questions.len() >= min_questions,
        "only {} rated answer(s) so far; a set this small looks like evidence and moves on \
         noise. Keep using the dock and rating answers with Ctrl+G, or pass \
         --min-questions {}",
        questions.len(),
        questions.len().max(1)
    );

    let yaml = serde_yaml::to_string(&questions)?;
    let header = format!(
        "# Exported from {} rated answers by `brainctl bench-export`.\n\
         #\n\
         # `expected` is the set of sections that produced an answer you marked good, so\n\
         # these labels came from real questions rather than invented ones. Hand-correct\n\
         # them: a good answer can still have retrieved one irrelevant section alongside\n\
         # the right ones.\n\n",
        questions.len()
    );

    match out {
        Some(path) => {
            std::fs::write(path, format!("{header}{yaml}"))?;
            println!("wrote {} questions to {}", questions.len(), path.display());
        }
        None => print!("{header}{yaml}"),
    }
    Ok(())
}

/// A context that explicitly means "rank without context".
///
/// `DesktopContext::default()` is ambiguous — it is also what a client sends when it simply
/// has nothing to report, and the daemon fills that in from the last summon. This marks the
/// difference.
fn suppressed_context() -> brain_proto::DesktopContext {
    brain_proto::DesktopContext {
        wm_class: Some(brain_proto::NO_CONTEXT.to_string()),
        ..Default::default()
    }
}

/// Unresolved disagreements in the vault.
///
/// Not a pass/fail check — a contradiction is a note to write, not a broken install — so it
/// is reported separately from the `ok`/`FAIL` lines and never changes the exit code. Two
/// sections of your own vault that disagree, with nothing marking which one won, is exactly
/// what makes the dock answer confidently and wrongly: whichever one BM25 happens to prefer
/// becomes the truth.
fn report_vault_health(config: &brain_core::Config) {
    for source in config.vaults() {
        let Ok(database) = yalive::db::Database::open(&source.path) else {
            continue;
        };
        let Ok(pairs) = database.unresolved_contradictions() else {
            continue;
        };
        if pairs.is_empty() {
            println!("ok    vault {:?} has no unresolved contradictions", source.name);
            continue;
        }

        println!(
            "note  vault {:?} has {} unresolved contradiction(s):",
            source.name,
            pairs.len()
        );
        for pair in &pairs {
            println!(
                "      {} ({}) ⟷ {} ({})",
                pair.left_heading,
                pair.left_path.display(),
                pair.right_heading,
                pair.right_path.display()
            );
        }
        println!("      mark one `status: obsolete`, or add `supersedes:: [[…]]` to the winner");
    }
}

fn load_config() -> Result<brain_core::Config> {
    match std::env::var_os("BRAIN_CONFIG") {
        Some(path) => brain_core::Config::load_from(std::path::Path::new(&path)),
        None => brain_core::Config::load(),
    }
    .map_err(anyhow::Error::new)
    .context("loading the configuration")
}

/// Environment checks that do not need the daemon.
///
/// Every failure here has a specific, actionable fix. "The dock looks flat and
/// square" is almost always a dead compositor, not a UI bug — so check for one.
async fn doctor() -> Result<()> {
    let mut problems = 0;

    let mut check = |label: &str, ok: bool, hint: &str| {
        if ok {
            println!("ok    {label}");
        } else {
            println!("FAIL  {label}\n      {hint}");
            problems += 1;
        }
    };

    match socket_path() {
        Ok(path) => {
            let running = tokio::net::UnixStream::connect(&path).await.is_ok();
            check(
                "brain-daemon reachable",
                running,
                "start it with `brain-daemon`",
            );
        }
        Err(err) => check("control socket path", false, &err.to_string()),
    }

    check(
        "compositor running (picom)",
        is_running("picom") || is_running("compton"),
        "start it with `picom -b` — without it the dock has no rounded corners, \
         transparency, or shadow",
    );

    // The config is checked here rather than only at daemon startup, because a daemon that
    // refused to start is exactly when someone runs `doctor` — and it should say why.
    match load_config() {
        Ok(config) => {
            check("configuration loads", true, "");

            for source in &config.sources {
                let readable = source.path.is_dir();
                check(
                    &format!("source {:?} readable", source.name),
                    readable,
                    &format!("{} is not a directory", source.path.display()),
                );
                if readable {
                    match count_matching(source) {
                        // A source whose globs match nothing looks identical to a working
                        // one until a query comes back empty.
                        Ok(0) => check(
                            &format!("source {:?} matches files", source.name),
                            false,
                            "the include globs match nothing in that directory",
                        ),
                        Ok(count) => {
                            println!("ok    source {:?} matches {count} files", source.name);
                            // Spec §29: a source this size is almost always a mistake.
                            if count > 50_000 {
                                println!(
                                    "warn  source {:?} would index {count} files; \
                                     narrow it with include/exclude",
                                    source.name
                                );
                            }
                        }
                        Err(error) => check(
                            &format!("source {:?} walkable", source.name),
                            false,
                            &error.to_string(),
                        ),
                    }
                }
            }

            report_vault_health(&config);

            // Check the openers actually configured, not a hardcoded list — a config
            // pointing at `kitty` should not fail because `ghostty` is missing.
            for (name, template) in brain_engine::actions::all_openers(&config.openers) {
                let program = template.first().cloned().unwrap_or_default();
                check(
                    &format!("opener {name} ({program})"),
                    brain_engine::actions::opener_is_installed(template),
                    &format!("{program} is not on PATH; fix [openers] {name}"),
                );
            }
        }
        Err(error) => check("configuration loads", false, &format!("{error:#}")),
    }

    if problems == 0 {
        println!("\nall checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} check(s) failed")
    }
}

fn is_running(process: &str) -> bool {
    std::process::Command::new("pgrep")
        .arg("-x")
        .arg(process)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
