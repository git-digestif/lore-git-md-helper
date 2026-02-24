use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::rag_parse;

/// Ingest a single markdown email file into the database.
///
/// The file path is used as the primary key (`path` column).  When
/// ingesting from disk rather than a git tree, `blob_sha` is left
/// empty; it will be filled in by the git-backed ingest path later.
pub fn ingest_file(conn: &Connection, path: &Path) -> Result<()> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    ingest_str(conn, &path.to_string_lossy(), "", &src)
}

/// Ingest pre-loaded markdown content with an explicit path key and
/// optional blob SHA.  Used by both the file-based and git-based
/// ingest paths.
pub fn ingest_str(conn: &Connection, path: &str, blob_sha: &str, src: &str) -> Result<()> {
    let e = rag_parse::parse_email(src);
    conn.execute(
        "INSERT OR REPLACE INTO emails
         (path, blob_sha, subject, author, date, message_id, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            path,
            blob_sha,
            e.subject,
            e.author,
            e.date,
            e.message_id,
            e.body
        ],
    )
    .with_context(|| format!("failed to insert {path}"))?;
    Ok(())
}
