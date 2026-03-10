use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::{cat_file::CatFile, git_util::resolve_ref, rag_db, rag_git, rag_parse};

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

/// Ingest all markdown emails from a git repository tree.
///
/// Compares the tree at `git_ref` against what is already in the
/// database (via blob SHAs) and only processes changed or new files.
/// `on_scan` is called during the initial tree scan with `(count, path)`
/// for each entry; `on_progress` during blob processing with
/// `(done, total)`.  Returns the number of emails upserted.
pub fn ingest_repo(
    conn: &Connection,
    repo: &str,
    git_ref: &str,
    on_scan: impl FnMut(usize, &str),
    mut on_progress: impl FnMut(usize, usize),
) -> Result<usize> {
    use std::time::Instant;

    let current = resolve_ref(repo, git_ref).context(format!("failed to resolve {git_ref}"))?;
    let last_commit = rag_db::get_state(conn, "last_commit");
    if last_commit.as_deref() == Some(&current) {
        return Ok(0);
    }

    // When a previous commit is known, use diff-tree to find only the
    // changed paths.  This avoids the full ls-tree scan and the full
    // DB load that dominate incremental ingest time on large repos.
    let (to_upsert, to_delete) = if let Some(ref old) = last_commit {
        let t = Instant::now();
        let entries = rag_git::diff_tree(repo, old, &current, on_scan)?;
        let mut upsert = Vec::new();
        let mut delete = Vec::new();
        for e in &entries {
            if e.deleted {
                delete.push(e.path.clone());
            } else {
                upsert.push((e.path.clone(), e.new_sha.clone()));
            }
        }
        eprintln!(
            "\rdiffed {} changes in {:.1}s; {} to upsert, {} to delete",
            entries.len(),
            t.elapsed().as_secs_f64(),
            upsert.len(),
            delete.len(),
        );
        (upsert, delete)
    } else {
        // Cold start: full tree scan + DB load
        let t = Instant::now();
        let tree = rag_git::ls_tree(repo, git_ref, on_scan)?;
        eprintln!(
            "\rscanned {} emails in {:.1}s, loading database...",
            tree.len(),
            t.elapsed().as_secs_f64(),
        );

        let t = Instant::now();
        let mut existing: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT path, blob_sha FROM emails")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for r in rows {
                let (p, s) = r?;
                existing.insert(p, s);
            }
        }

        let upsert: Vec<(String, String)> = tree
            .iter()
            .filter(|(path, sha)| existing.get(*path) != Some(*sha))
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect();

        let delete: Vec<String> = existing
            .keys()
            .filter(|p| !tree.contains_key(p.as_str()))
            .cloned()
            .collect();

        eprintln!(
            "loaded {} existing in {:.1}s; {} to upsert, {} to delete",
            existing.len(),
            t.elapsed().as_secs_f64(),
            upsert.len(),
            delete.len(),
        );
        (upsert, delete)
    };

    // Delete removed paths
    if !to_delete.is_empty() {
        let t = Instant::now();
        let placeholders: String = to_delete.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM emails WHERE path IN ({placeholders})");
        let params: Vec<&dyn rusqlite::types::ToSql> = to_delete
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        conn.execute(&sql, params.as_slice())?;
        eprintln!(
            "deleted {} in {:.1}s",
            to_delete.len(),
            t.elapsed().as_secs_f64()
        );
    }

    let total = to_upsert.len();
    if total == 0 {
        rag_db::set_state(conn, "last_commit", &current)?;
        return Ok(0);
    }

    // Fetch and ingest via CatFile
    let t = Instant::now();
    let mut cat = CatFile::new(repo)?;
    let mut failed = 0usize;
    for (i, (path, sha)) in to_upsert.iter().enumerate() {
        match cat.get_str(sha) {
            Some(content) => ingest_str(conn, path, sha, &content)?,
            None => {
                eprintln!("warning: could not read blob {sha} for {path}, skipping");
                failed += 1;
            }
        }
        on_progress(i + 1, total);
    }
    eprintln!(
        "processed {total} emails in {:.1}s",
        t.elapsed().as_secs_f64()
    );

    if failed > 0 {
        anyhow::bail!(
            "{failed} of {total} blobs could not be read; \
             not advancing last_commit so they will be retried"
        );
    }

    rag_db::set_state(conn, "last_commit", &current)?;

    // Track accumulated inserts since the last FTS optimize, and only
    // run the expensive segment merge when enough rows have piled up.
    let unoptimized: usize = rag_db::get_state(conn, "unoptimized_inserts")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        + total;
    if unoptimized >= 5000 {
        let t = Instant::now();
        eprintln!("optimizing search index...");
        conn.execute("INSERT INTO emails_fts(emails_fts) VALUES ('optimize')", [])?;
        eprintln!("optimized in {:.1}s", t.elapsed().as_secs_f64());
        rag_db::set_state(conn, "unoptimized_inserts", "0")?;
    } else {
        rag_db::set_state(conn, "unoptimized_inserts", &unoptimized.to_string())?;
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast_import::FastImport;
    use crate::git_util::tests::init_bare_repo;

    fn setup() -> (tempfile::TempDir, String, Connection) {
        let dir = init_bare_repo();
        let repo = dir.path().to_str().unwrap().to_string();
        let conn = crate::rag_db::open(":memory:").unwrap();
        (dir, repo, conn)
    }

    #[test]
    fn ingest_repo_happy_path() {
        let (dir, repo, conn) = setup();

        let mut fi = FastImport::new(&repo, "refs/heads/main").unwrap();
        fi.commit(
            "seed",
            &[(
                "2025/01/01/00-00-00.md",
                "# Test\n\n| Header | Value |\n|--|--|\n| **From** | alice |\n\n---\n\nBody.\n",
            )],
        )
        .unwrap();
        fi.finish().unwrap();

        let count = ingest_repo(&conn, &repo, "refs/heads/main", |_, _| {}, |_, _| {}).unwrap();
        assert_eq!(count, 1);

        // Verify the email was inserted
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);

        // Second run should be a no-op (same commit)
        let count2 = ingest_repo(&conn, &repo, "refs/heads/main", |_, _| {}, |_, _| {}).unwrap();
        assert_eq!(count2, 0);

        drop(dir);
    }

    #[test]
    fn ingest_str_populates_fields() {
        let conn = crate::rag_db::open(":memory:").unwrap();
        ingest_str(
            &conn,
            "test.md",
            "sha1",
            "# My Subject\n\n| Header | Value |\n|--|--|\n| **From** | Bob |\n| **Message-ID** | [abc@example.com](https://lore.kernel.org/git/abc@example.com) |\n\n---\n\nSome body text.\n",
        )
        .unwrap();

        let (subject, author, message_id): (String, String, String) = conn
            .query_row(
                "SELECT subject, author, message_id FROM emails WHERE path = ?1",
                ["test.md"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(subject, "My Subject");
        assert_eq!(author, "Bob");
        assert_eq!(message_id, "abc@example.com");
    }
}
