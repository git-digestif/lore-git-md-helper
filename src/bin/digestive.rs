use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use lore_git_md_helper::ai_backend::BackendArgs;
use lore_git_md_helper::digestive::{Digestive, redo_daily, resummarize_thread};
use lore_git_md_helper::git_util::last_digest_day;

#[derive(Parser)]
#[command(about = "Batch-summarize Git mailing list emails in a bare repository")]
struct Args {
    /// Path to the target bare repository.
    #[arg(long)]
    target_repo: String,

    /// Only process emails at or after this date-key prefix (e.g. "2025/06/15").
    #[arg(long)]
    since: Option<String>,

    /// Only process emails strictly before this date-key prefix.
    #[arg(long)]
    until: Option<String>,

    /// Number of emails per fast-import commit (default: 5).
    #[arg(long, default_value_t = 5)]
    batch_size: usize,

    /// Git ref to read from and write to (default: refs/heads/main).
    #[arg(long, default_value = "refs/heads/main")]
    git_ref: String,

    /// Print what would be done without calling AI or writing to the repo.
    #[arg(long)]
    dry_run: bool,

    /// Soft deadline for the wall-clock runtime, parsed in humantime
    /// format (e.g. "50m", "1h30m").  When the deadline is reached
    /// during the email-summarization loop, `digestive` finishes the
    /// current iteration, flushes its work, prints a clear notice,
    /// and exits with status 0.  Intended for hourly workflows that
    /// want to bound a single run and let the next scheduled run
    /// resume from the last committed state.
    #[arg(long)]
    max_runtime: Option<humantime::Duration>,

    #[command(flatten)]
    backend: BackendArgs,

    /// Optional retroactive-fix subcommand.  When omitted, `digestive`
    /// runs the batch summarization pipeline as before, preserving
    /// backward compatibility with existing CI invocations.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Rebuild one thread's `.thread.ai.md` and `.thread.human.md`
    /// from scratch by walking its emails in chronological order and
    /// feeding each per-email `.ai.md` back through the thread agent.
    /// Discards any confabulation baked into the existing summary.
    ResummarizeThread {
        /// The thread root's date-key (e.g. "2026/06/08/18-37-18").
        root_dk: String,
    },
    /// Regenerate `digest.human.md` and `digest.ai.md` for a given
    /// day using the current tip's per-email `.ai.md` files and the
    /// previous day's digest tree as the "before" state.
    RedoDaily {
        /// The day (e.g. "2026/07/02").
        day: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Consume `backend` out of `args` before matching on `cmd`, since
    // `BackendArgs::resolve` takes ownership.
    let Args {
        target_repo,
        since,
        until,
        batch_size,
        git_ref,
        dry_run,
        max_runtime,
        backend: backend_args,
        cmd,
    } = args;

    let backend = if !dry_run {
        Some(backend_args.resolve()?)
    } else {
        None
    };

    match cmd {
        None => {
            run_pipeline(
                &target_repo,
                &git_ref,
                since,
                until,
                batch_size,
                max_runtime,
                backend.as_ref(),
                dry_run,
            )
            .await
        }
        Some(Cmd::ResummarizeThread { root_dk }) => {
            let backend = backend
                .as_ref()
                .context("--dry-run is not supported for resummarize-thread")?;
            resummarize_thread(&target_repo, &git_ref, &root_dk, backend).await
        }
        Some(Cmd::RedoDaily { day }) => {
            let backend = backend
                .as_ref()
                .context("--dry-run is not supported for redo-daily")?;
            redo_daily(&target_repo, &git_ref, &day, backend).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    target_repo: &str,
    git_ref: &str,
    since: Option<String>,
    until: Option<String>,
    batch_size: usize,
    max_runtime: Option<humantime::Duration>,
    backend: Option<&lore_git_md_helper::ai_backend::Backend>,
    dry_run: bool,
) -> Result<()> {
    let since = since.or_else(|| {
        let day = last_digest_day(target_repo, git_ref)?;
        eprintln!("[digestive] resuming after {day}");
        Some(day)
    });

    let mut d = Digestive::new(target_repo, git_ref, batch_size, backend, dry_run)?;

    if let Some(duration) = max_runtime {
        let duration: std::time::Duration = duration.into();
        let deadline = std::time::Instant::now() + duration;
        eprintln!(
            "[digestive] --max-runtime {} set; will exit cleanly when reached",
            humantime::format_duration(duration),
        );
        d = d.with_deadline(deadline);
    }

    d.run(since.as_deref(), until.as_deref()).await?;
    let result = d.finish()?;

    eprintln!(
        "[digestive] Done: {} emails summarized",
        result.total_processed,
    );

    Ok(())
}
