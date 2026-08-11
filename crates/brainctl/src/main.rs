//! `brainctl` — one request, one answer, exit.
//!
//! This is what i3 binds to. It must start fast and it must fail loudly: a
//! silent no-op when the daemon is down would look exactly like a broken
//! keybinding, and you would go looking in the wrong place.

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
    },
    /// Print daemon diagnostics (spec §38).
    Status,
    Reindex,
    PauseIndexing,
    ResumeIndexing,
    /// Check the environment: socket, compositor, openers, model.
    Doctor,
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
    if let Command::Doctor = command {
        return doctor().await;
    }

    let mut connection = connect().await?;

    match command {
        Command::Toggle => connection.send(&ClientRequest::Toggle).await?,
        Command::Show => connection.send(&ClientRequest::Show).await?,
        Command::Hide => connection.send(&ClientRequest::Hide).await?,
        Command::PauseIndexing => connection.send(&ClientRequest::PauseIndexing).await?,
        Command::ResumeIndexing => connection.send(&ClientRequest::ResumeIndexing).await?,

        Command::Status => {
            connection.send(&ClientRequest::Status).await?;
            return print_status(&mut connection).await;
        }
        Command::Reindex => {
            connection.send(&ClientRequest::Reindex).await?;
            return drain_until_complete(&mut connection).await;
        }
        Command::Ask { query, no_llm } => {
            let text = query.join(" ");
            anyhow::ensure!(!text.trim().is_empty(), "no question given");
            connection
                .send(&ClientRequest::Query {
                    id: Uuid::new_v4(),
                    text,
                    context: Default::default(),
                    retrieval_only: no_llm,
                })
                .await?;
            return stream_answer(&mut connection).await;
        }
        Command::Doctor => unreachable!("handled above"),
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

async fn drain_until_complete(connection: &mut ClientConnection) -> Result<()> {
    while let Some(event) = connection.recv().await {
        match event? {
            ServerEvent::Complete { .. } => return Ok(()),
            ServerEvent::Error { message, .. } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    Ok(())
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

    for (binary, purpose) in [
        ("nvim", "opening notes at a line"),
        ("ghostty", "hosting the editor"),
        ("xdg-open", "opening urls"),
        ("gtk-launch", "launching desktop applications"),
    ] {
        check(
            &format!("{binary} on PATH"),
            which(binary),
            &format!("needed for {purpose}"),
        );
    }

    if problems == 0 {
        println!("\nall checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} check(s) failed")
    }
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
        })
        .unwrap_or(false)
}

fn is_running(process: &str) -> bool {
    std::process::Command::new("pgrep")
        .arg("-x")
        .arg(process)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
