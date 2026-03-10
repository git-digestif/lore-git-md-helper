use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use lore_git_md_helper::{ai_backend::BackendArgs, rag_db, rag_ingest, rag_query};

/// RAG over the Git mailing list archive.
#[derive(Parser)]
#[command(name = "lore-rag")]
enum Cli {
    /// Index markdown email file(s) into the database.
    Ingest(IngestArgs),
    /// Query the database and answer using an AI backend.
    Query(QueryArgs),
}

#[derive(clap::Args)]
struct IngestArgs {
    /// SQLite database path (created if absent).
    #[arg(long, default_value = "lore-git.db")]
    db: PathBuf,

    /// Git repository to ingest from (alternative to listing files).
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Git ref to ingest (used with --repo).
    #[arg(long, default_value = "HEAD")]
    git_ref: String,

    /// Markdown file(s) to ingest (alternative to --repo).
    files: Vec<PathBuf>,
}

#[derive(clap::Args)]
struct QueryArgs {
    /// SQLite database path.
    #[arg(long, default_value = "lore-git.db")]
    db: PathBuf,

    /// Number of emails to retrieve.
    #[arg(long, default_value_t = 15)]
    top_k: usize,

    /// Maximum characters per email excerpt in the prompt.
    #[arg(long, default_value_t = 1200)]
    max_excerpt: usize,

    /// Print the assembled prompt instead of sending it to a model.
    #[arg(long)]
    print_prompt: bool,

    #[command(flatten)]
    backend: BackendArgs,

    /// The question to answer.
    #[arg(required = true)]
    question: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Ingest(args) => {
            let conn = rag_db::open(args.db.to_str().unwrap())?;
            if let Some(repo) = &args.repo {
                let mut last_scan_print = std::time::Instant::now();
                let start = std::time::Instant::now();
                let mut last_print = start;
                let n = rag_ingest::ingest_repo(
                    &conn,
                    repo.to_str().unwrap(),
                    &args.git_ref,
                    |count, path| {
                        let now = std::time::Instant::now();
                        if now.duration_since(last_scan_print).as_millis() < 250 {
                            return;
                        }
                        last_scan_print = now;
                        let date = &path[..path.len().min(10)];
                        eprint!("\rscanning: {count} emails ({date})   ");
                    },
                    |done, total| {
                        let now = std::time::Instant::now();
                        if done < total && now.duration_since(last_print).as_millis() < 250 {
                            return;
                        }
                        last_print = now;
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = done as f64 / elapsed;
                        let remaining = (total - done) as f64 / rate;
                        let mins = remaining as u64 / 60;
                        let secs = remaining as u64 % 60;
                        eprint!("\r{done}/{total}  {rate:.0} emails/s  ETA {mins}m{secs:02}s   ");
                    },
                )?;
                eprintln!();
                if n == 0 {
                    eprintln!("already up to date");
                } else {
                    eprintln!("indexed {n} emails");
                }
            } else if args.files.is_empty() {
                anyhow::bail!("provide --repo or one or more files");
            } else {
                for path in &args.files {
                    rag_ingest::ingest_file(&conn, path)?;
                    eprintln!("indexed: {}", path.display());
                }
            }
        }
        Cli::Query(args) => {
            let conn = rag_db::open(args.db.to_str().unwrap())?;
            let question = args.question.join(" ");

            let results = rag_query::retrieve(&conn, &question, args.top_k)?;
            if results.is_empty() {
                eprintln!("no results found");
                return Ok(());
            }

            let prompt = rag_query::build_prompt(&question, &results, args.max_excerpt);

            if args.print_prompt {
                println!("{prompt}");
                return Ok(());
            }

            let backend = args.backend.resolve()?;

            let system = "You are answering questions about the Git version-control \
                          project based on emails from the git@vger.kernel.org mailing list.";
            let answer = backend.chat(system, &prompt).await?;
            println!("{answer}");
        }
    }
    Ok(())
}
