use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use lore_git_md_helper::{rag_db, rag_ingest, rag_query};

/// RAG over the Git mailing list archive.
#[derive(Parser)]
#[command(name = "lore-rag")]
enum Cli {
    /// Index markdown email file(s) into the database.
    Ingest(IngestArgs),
    /// Query the database and print the augmented prompt.
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

    /// The question to answer.
    #[arg(required = true)]
    question: Vec<String>,
}

fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Ingest(args) => {
            let conn = rag_db::open(args.db.to_str().unwrap())?;
            if let Some(repo) = &args.repo {
                let n = rag_ingest::ingest_repo(
                    &conn,
                    repo.to_str().unwrap(),
                    &args.git_ref,
                    |done, total| {
                        eprint!("\r{done}/{total}");
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
            println!("{prompt}");
        }
    }
    Ok(())
}
