use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use lore_git_md_helper::{rag_db, rag_ingest};

/// RAG over the Git mailing list archive.
#[derive(Parser)]
#[command(name = "lore-rag")]
enum Cli {
    /// Index markdown email file(s) into the database.
    Ingest(IngestArgs),
}

#[derive(clap::Args)]
struct IngestArgs {
    /// SQLite database path (created if absent).
    #[arg(long, default_value = "lore-git.db")]
    db: PathBuf,

    /// Markdown file(s) to ingest.
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Ingest(args) => {
            let conn = rag_db::open(args.db.to_str().unwrap())?;
            for path in &args.files {
                rag_ingest::ingest_file(&conn, path)?;
                eprintln!("indexed: {}", path.display());
            }
        }
    }
    Ok(())
}
