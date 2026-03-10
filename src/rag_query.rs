use anyhow::Result;
use regex::Regex;
use rusqlite::Connection;
use std::sync::OnceLock;

pub struct EmailResult {
    pub path: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub message_id: String,
    pub body: String,
}

/// Retrieve up to `limit` emails matching `query` using FTS5 BM25 ranking.
pub fn retrieve(conn: &Connection, query: &str, limit: usize) -> Result<Vec<EmailResult>> {
    static RE_WORD: OnceLock<Regex> = OnceLock::new();
    let re = RE_WORD.get_or_init(|| Regex::new(r"\w{2,}").unwrap());

    // Build an OR query from individual words so partial matches still hit.
    let fts_query: String = re
        .find_iter(query)
        .map(|m| format!("\"{}\"", m.as_str()))
        .collect::<Vec<_>>()
        .join(" OR ");

    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let mut stmt = conn.prepare(
        "SELECT e.path, e.subject, e.author, e.date, e.message_id, e.body
         FROM emails_fts f
         JOIN emails e ON e.id = f.rowid
         WHERE emails_fts MATCH ?1
         ORDER BY f.rank
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
        Ok(EmailResult {
            path: row.get(0)?,
            subject: row.get(1).unwrap_or_default(),
            author: row.get(2).unwrap_or_default(),
            date: row.get(3).unwrap_or_default(),
            message_id: row.get(4).unwrap_or_default(),
            body: row.get(5).unwrap_or_default(),
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// Build the RAG prompt from a question and retrieved email results.
///
/// Each email is labelled by its Message-ID so the model can cite
/// sources precisely with `[<message-id>]`.
pub fn build_prompt(question: &str, results: &[EmailResult], max_excerpt: usize) -> String {
    let mut prompt = format!(
        "You are answering questions about the Git version-control project \
         based on emails from the git@vger.kernel.org mailing list.\n\
         Use ONLY the provided email excerpts as evidence.\n\
         Cite emails by their Message-ID in angle brackets, e.g. \
         [<message-id@example.com>].\n\
         \n\
         Question: {question}\n\
         \n\
         --- Context ({n} emails, ranked by relevance) ---\n",
        question = question,
        n = results.len(),
    );

    for r in results {
        let excerpt: String = r.body.chars().take(max_excerpt).collect();
        let ellipsis = if r.body.chars().count() > max_excerpt {
            " …"
        } else {
            ""
        };
        prompt.push_str(&format!(
            "\n[<{msgid}>] {path}\nSubject: {subj}\nFrom: {author}   Date: {date}\n\n{excerpt}{ellipsis}\n",
            msgid  = r.message_id,
            path   = r.path,
            subj   = r.subject,
            author = r.author,
            date   = r.date,
        ));
    }

    prompt.push_str(&format!(
        "\n--- End of context ---\n\nAnswer: {question}",
        question = question,
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        crate::rag_db::open(":memory:").unwrap()
    }

    #[test]
    fn retrieve_empty_query_returns_nothing() {
        let conn = setup_db();
        let results = retrieve(&conn, "", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn retrieve_no_matches() {
        let conn = setup_db();
        crate::rag_ingest::ingest_str(
            &conn,
            "2025/01/01/00-00-00.md",
            "abc123",
            "# Subject\n\n| Header | Value |\n|--|--|\n| **From** | alice |\n\n---\n\nHello world.\n",
        )
        .unwrap();
        let results = retrieve(&conn, "nonexistent-xyzzy", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn retrieve_finds_matching_email() {
        let conn = setup_db();
        crate::rag_ingest::ingest_str(
            &conn,
            "2025/01/01/00-00-00.md",
            "abc123",
            "# Rebase improvements\n\n| Header | Value |\n|--|--|\n| **From** | alice |\n\n---\n\nDiscussing interactive rebase.\n",
        )
        .unwrap();
        let results = retrieve(&conn, "interactive rebase", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].subject.contains("Rebase"));
    }

    #[test]
    fn build_prompt_includes_question_and_context() {
        let results = vec![EmailResult {
            path: "2025/01/01/00-00-00.md".into(),
            subject: "Test Subject".into(),
            author: "Alice".into(),
            date: "2025-01-01".into(),
            message_id: "test@example.com".into(),
            body: "Some email body content here.".into(),
        }];
        let prompt = build_prompt("What is this about?", &results, 100);
        assert!(prompt.contains("What is this about?"));
        assert!(prompt.contains("<test@example.com>"));
        assert!(prompt.contains("Test Subject"));
        assert!(prompt.contains("Some email body content here."));
    }

    #[test]
    fn build_prompt_truncates_long_bodies() {
        let results = vec![EmailResult {
            path: "x.md".into(),
            subject: "S".into(),
            author: "A".into(),
            date: "D".into(),
            message_id: "m@e".into(),
            body: "word ".repeat(100),
        }];
        let prompt = build_prompt("q", &results, 10);
        assert!(prompt.contains("word word"));
        assert!(prompt.contains(" …"));
    }
}
