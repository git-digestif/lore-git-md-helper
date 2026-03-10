use anyhow::{Context, Result};
use rusqlite::Connection;

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("failed to open database {path}"))?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS emails (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            path       TEXT UNIQUE NOT NULL,
            blob_sha   TEXT NOT NULL,
            subject    TEXT,
            author     TEXT,
            date       TEXT,
            message_id TEXT,
            body       TEXT
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(
            subject, author, body,
            content=emails,
            content_rowid=id
        );

        CREATE TRIGGER IF NOT EXISTS emails_ai AFTER INSERT ON emails BEGIN
            INSERT INTO emails_fts(rowid, subject, author, body)
            VALUES (new.id, new.subject, new.author, new.body);
        END;

        CREATE TRIGGER IF NOT EXISTS emails_ad AFTER DELETE ON emails BEGIN
            INSERT INTO emails_fts(emails_fts, rowid, subject, author, body)
            VALUES ('delete', old.id, old.subject, old.author, old.body);
        END;

        CREATE TRIGGER IF NOT EXISTS emails_au AFTER UPDATE ON emails BEGIN
            INSERT INTO emails_fts(emails_fts, rowid, subject, author, body)
            VALUES ('delete', old.id, old.subject, old.author, old.body);
            INSERT INTO emails_fts(rowid, subject, author, body)
            VALUES (new.id, new.subject, new.author, new.body);
        END;

        CREATE TABLE IF NOT EXISTS ingest_state (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
    ",
    )
    .context("failed to initialise schema")
}

pub fn get_state(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM ingest_state WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .ok()
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO ingest_state (key, value) VALUES (?1, ?2)",
        [key, value],
    )
    .context("failed to set ingest_state")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn schema_creates_tables() {
        let conn = mem();
        // emails table
        conn.execute(
            "INSERT INTO emails (path, blob_sha) VALUES ('a/b.md', 'abc')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn state_roundtrip() {
        let conn = mem();
        assert!(get_state(&conn, "last_commit").is_none());
        set_state(&conn, "last_commit", "deadbeef").unwrap();
        assert_eq!(get_state(&conn, "last_commit").as_deref(), Some("deadbeef"));
        set_state(&conn, "last_commit", "cafebabe").unwrap();
        assert_eq!(get_state(&conn, "last_commit").as_deref(), Some("cafebabe"));
    }

    #[test]
    fn fts5_virtual_table_exists() {
        let conn = mem();
        // If FTS5 is missing this will error
        conn.execute(
            "INSERT INTO emails (path, blob_sha, subject, author, body)
             VALUES ('x.md', 'sha1', 'subj', 'auth', 'some body text')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM emails_fts WHERE emails_fts MATCH 'body'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
