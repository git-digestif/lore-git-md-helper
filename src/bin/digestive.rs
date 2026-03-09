use anyhow::Result;
use clap::Parser;

use lore_git_md_helper::ai_backend::BackendArgs;
use lore_git_md_helper::digestive::Digestive;

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

    #[command(flatten)]
    backend: BackendArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let backend = if !args.dry_run {
        Some(args.backend.resolve()?)
    } else {
        None
    };

    let mut d = Digestive::new(
        &args.target_repo,
        &args.git_ref,
        args.batch_size,
        backend.as_ref(),
        args.dry_run,
    )?;

    d.run(args.since.as_deref(), args.until.as_deref()).await?;
    let result = d.finish()?;

    eprintln!(
        "[digestive] Done: {} emails summarized",
        result.total_processed,
    );

    Ok(())
}
