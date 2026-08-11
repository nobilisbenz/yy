//! `brain-bench` — run the retrieval benchmark.
//!
//! ```bash
//! brain-bench --vault ~/brain --questions benchmarks/retrieval.yaml
//! brain-bench --sweep search.bm25_heading=4,8,12
//! brain-bench --baseline benchmarks/results/last.json
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use brain_bench::{Report, load_questions, run, with_override};
use brain_core::Config;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "brain-bench", version, about = "Retrieval quality benchmark")]
struct Args {
    /// Vault to search. Defaults to the first vault source in the config.
    #[arg(long)]
    vault: Option<PathBuf>,

    /// The question set.
    #[arg(long, default_value = "benchmarks/retrieval.yaml")]
    questions: PathBuf,

    /// Config file. Defaults to `$XDG_CONFIG_HOME/brain/config.toml`, then to built-in
    /// defaults so the benchmark runs on a machine with no config at all.
    #[arg(long, env = "BRAIN_CONFIG")]
    config: Option<PathBuf>,

    /// Grid-search one config key: `search.bm25_heading=4,8,12`.
    ///
    /// Repeatable; every combination is run. This is the whole reason the ranking weights
    /// live in config rather than as literals.
    #[arg(long)]
    sweep: Vec<String>,

    /// A previous `--json` report to diff against.
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// Write the full report here, for use as a later `--baseline`.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Fail if Recall@3 is below this. For wiring into CI.
    #[arg(long)]
    floor: Option<f64>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("brain-bench: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<ExitCode> {
    let args = Args::parse();

    let config = match &args.config {
        Some(path) => Config::load_from(path).context("loading the configuration")?,
        None => Config::load().unwrap_or_else(|_| {
            eprintln!("brain-bench: no config found; using defaults");
            Config::default()
        }),
    };

    let vault = match &args.vault {
        Some(vault) => vault.clone(),
        None => config
            .vaults()
            .next()
            .map(|source| source.path.clone())
            .context("no vault configured and --vault not given")?,
    };

    let questions = load_questions(&args.questions)?;
    anyhow::ensure!(!questions.is_empty(), "the question set is empty");

    let runtime = tokio::runtime::Runtime::new()?;
    let combinations = expand_sweep(&args.sweep)?;

    let mut best: Option<(String, Report)> = None;
    for combination in &combinations {
        let mut candidate = config.clone();
        for (key, value) in combination {
            candidate = with_override(&candidate, key, value)?;
        }

        let label = if combination.is_empty() {
            "baseline".to_string()
        } else {
            combination
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" ")
        };

        let report = runtime.block_on(run(&vault, &candidate, &questions))?;
        println!("{}\n", report.render(&label));

        // Ranked by MRR rather than recall: the dock shows one primary source, so being
        // first is the outcome that matters and recall@5 cannot see it.
        if best.as_ref().is_none_or(|(_, b)| report.mrr > b.mrr) {
            best = Some((label, report));
        }
    }

    let (label, report) = best.context("no runs completed")?;

    if combinations.len() > 1 {
        println!("best by MRR: {label}\n");
    }

    if let Some(path) = &args.baseline {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let previous: Report = serde_json::from_str(&text)?;
        let moved = report.diff(&previous);

        if moved.is_empty() {
            println!("no question changed rank against the baseline");
        } else {
            println!("changed against the baseline:");
            for movement in &moved {
                println!("{}", movement.render());
            }
        }
        println!();
    }

    if let Some(path) = &args.json {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
        println!("report written to {}", path.display());
    }

    if let Some(floor) = args.floor
        && report.recall_at_3 < floor
    {
        eprintln!(
            "brain-bench: Recall@3 {:.2} is below the floor of {floor:.2}",
            report.recall_at_3
        );
        return Ok(ExitCode::FAILURE);
    }

    Ok(ExitCode::SUCCESS)
}

/// Turn `["a=1,2", "b=x,y"]` into every combination of overrides.
fn expand_sweep(sweep: &[String]) -> Result<Vec<Vec<(String, String)>>> {
    if sweep.is_empty() {
        return Ok(vec![Vec::new()]);
    }

    let mut combinations: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for entry in sweep {
        let (key, values) = entry
            .split_once('=')
            .with_context(|| format!("`{entry}` is not `key=value,value`"))?;

        let mut next = Vec::new();
        for combination in &combinations {
            for value in values.split(',') {
                let mut extended = combination.clone();
                extended.push((key.to_string(), value.trim().to_string()));
                next.push(extended);
            }
        }
        combinations = next;
    }
    Ok(combinations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_expands_to_every_combination() {
        let combinations =
            expand_sweep(&["a=1,2".to_string(), "b=x,y".to_string()]).unwrap();
        assert_eq!(combinations.len(), 4);
        assert!(combinations.contains(&vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "y".to_string())
        ]));
    }

    #[test]
    fn no_sweep_means_one_run_with_the_config_as_written() {
        let combinations = expand_sweep(&[]).unwrap();
        assert_eq!(combinations.len(), 1);
        assert!(combinations[0].is_empty());
    }

    #[test]
    fn a_malformed_sweep_is_refused() {
        assert!(expand_sweep(&["no-equals-sign".to_string()]).is_err());
    }
}
