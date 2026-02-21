//! Write a git fast-import stream from batch processing results.
//!
//! Generates a single commit on `refs/heads/main` containing:
//! - Markdown files for each email (`<date-key>.md`)
//! - Thread overview files (`<thread-root>.thread.md`)
//! - Symlinks from non-root thread entries to the root's thread file
//!
//! Also emits a notes commit via `notes_import::emit_notes_update`.

use crate::batch_import::BatchResult;
use crate::fast_import::FastImport;
use crate::msgid_map::MsgIdMap;
use crate::notes_import::emit_notes_update;
use crate::symlink::compute_relative_path;

/// Write a complete fast-import stream for one batch.
///
/// `mark` is incremented for each commit emitted; pass it across calls
/// to avoid mark collisions when writing multiple batches.
/// `main_head`: updated after each call — starts as `None` (no parent)
/// or `Some("HEAD".into())`, gets rewritten to `Some(":N")` pointing
/// at this batch's content commit mark.
/// `notes_head`: same for the notes commit on refs/notes/msgid.
/// `source_label`: used in the commit message (e.g. repo path).
///
/// Returns the number of notes entries written.
pub fn write_fast_import(
    content_fi: &mut FastImport,
    result: &BatchResult,
    map: &MsgIdMap,
    notes_fi: &mut FastImport,
    source_label: &str,
) -> anyhow::Result<usize> {
    let msg = match &result.last_source_commit {
        Some(oid) => format!(
            "Import {} emails from {source_label}\n\nSource-Commit: {oid}",
            result.emails.len(),
        ),
        None => format!("Import {} emails from {source_label}", result.emails.len()),
    };

    // Collect regular files: email .md + thread overview .md
    let mut files: Vec<(String, String)> = Vec::new();
    for email in &result.emails {
        files.push((format!("{}.md", email.date_key), email.markdown.clone()));
    }
    for (root_dk, tree) in &result.trees {
        files.push((format!("{root_dk}.thread.md"), tree.render(root_dk)));
    }

    // Collect symlinks for non-root thread entries
    let mut symlinks: Vec<(String, String)> = Vec::new();
    for (root_dk, tree) in &result.trees {
        for dk in tree.date_keys() {
            if dk == root_dk {
                continue;
            }
            let dk_dir = dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let root_path = format!("{root_dk}.thread.md");
            let target = compute_relative_path(dk_dir, &root_path);
            symlinks.push((format!("{dk}.thread.md"), target));
        }
    }

    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let sym_refs: Vec<(&str, &str)> = symlinks
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    content_fi.commit_with_symlinks(&msg, &file_refs, &sym_refs)?;

    // Notes commit
    let notes_count = emit_notes_update(notes_fi, map, result.last_source_commit.as_deref())?;

    Ok(notes_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch_import::{BatchResult, ProcessedEmail};
    use crate::thread_file::ThreadTree;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    #[derive(Clone)]
    struct TestBuf(Rc<RefCell<Vec<u8>>>);

    impl std::io::Write for TestBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(data)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    fn test_fi() -> (FastImport, Rc<RefCell<Vec<u8>>>) {
        let buf = Rc::new(RefCell::new(Vec::new()));
        let fi = FastImport::from_writer(TestBuf(buf.clone()), "refs/heads/main");
        (fi, buf)
    }

    #[test]
    fn test_write_fast_import_basic() {
        let mut trees = HashMap::new();
        let mut tree = ThreadTree::new();
        tree.insert("2025/02/12/04-10-17", None, "[PATCH] test", "Author");
        trees.insert("2025/02/12/04-10-17".to_string(), tree);

        let result = BatchResult {
            emails: vec![ProcessedEmail {
                date_key: "2025/02/12/04-10-17".into(),
                markdown: "# Test\n".into(),
                thread_root: "2025/02/12/04-10-17".into(),
            }],
            trees,
            skipped: 0,
            last_source_commit: None,
        };

        let map = MsgIdMap::new(None);
        let (mut content_fi, buf) = test_fi();
        let mut notes_fi = content_fi.sibling("refs/notes/msgid");
        write_fast_import(&mut content_fi, &result, &map, &mut notes_fi, "test").unwrap();

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(output.contains("commit refs/heads/main\n"));
        assert!(output.contains("M 100644 inline 2025/02/12/04-10-17.md\n"));
        assert!(output.contains("M 100644 inline 2025/02/12/04-10-17.thread.md\n"));
        assert!(!output.contains("\nfrom "));
    }

    #[test]
    fn test_source_commit_in_message() {
        let result = BatchResult {
            emails: vec![ProcessedEmail {
                date_key: "2025/02/12/04-10-17".into(),
                markdown: "# Test\n".into(),
                thread_root: "2025/02/12/04-10-17".into(),
            }],
            trees: HashMap::new(),
            skipped: 0,
            last_source_commit: Some("abc123def456".into()),
        };

        let map = MsgIdMap::new(None);
        let (mut content_fi, buf) = test_fi();
        let mut notes_fi = content_fi.sibling("refs/notes/msgid");
        write_fast_import(&mut content_fi, &result, &map, &mut notes_fi, "test").unwrap();

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            output.contains("Source-Commit: abc123def456"),
            "commit message should contain Source-Commit trailer"
        );
    }

    #[test]
    fn test_write_fast_import_with_symlinks() {
        let mut trees = HashMap::new();
        let mut tree = ThreadTree::new();
        tree.insert("2025/02/12/04-10-17", None, "cover", "A");
        tree.insert(
            "2025/02/12/04-10-18",
            Some("2025/02/12/04-10-17"),
            "patch",
            "A",
        );
        tree.insert(
            "2025/02/13/05-00-00",
            Some("2025/02/12/04-10-17"),
            "review",
            "B",
        );
        trees.insert("2025/02/12/04-10-17".to_string(), tree);

        let result = BatchResult {
            emails: vec![],
            trees,
            skipped: 0,
            last_source_commit: None,
        };

        let map = MsgIdMap::new(None);
        let (mut content_fi, buf) = test_fi();
        let mut notes_fi = content_fi.sibling("refs/notes/msgid");
        write_fast_import(&mut content_fi, &result, &map, &mut notes_fi, "test").unwrap();

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(output.contains("M 120000 inline 2025/02/12/04-10-18.thread.md"));
        assert!(output.contains("M 120000 inline 2025/02/13/05-00-00.thread.md"));
    }

    #[test]
    fn test_parents_chain_across_calls() {
        let make_result = |dk: &str| BatchResult {
            emails: vec![ProcessedEmail {
                date_key: dk.into(),
                markdown: "# Test\n".into(),
                thread_root: dk.into(),
            }],
            trees: HashMap::new(),
            skipped: 0,
            last_source_commit: None,
        };
        let map = MsgIdMap::new(None);
        let (mut content_fi, buf) = test_fi();
        content_fi.set_parent("HEAD".into());
        let mut notes_fi = content_fi.sibling("refs/notes/msgid");

        write_fast_import(
            &mut content_fi,
            &make_result("2025/01/01/00-00-00"),
            &map,
            &mut notes_fi,
            "a",
        )
        .unwrap();
        write_fast_import(
            &mut content_fi,
            &make_result("2025/01/02/00-00-00"),
            &map,
            &mut notes_fi,
            "b",
        )
        .unwrap();

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        // First batch parents from HEAD
        assert!(output.contains("from HEAD\n"));
        // Second batch parents from :1 (the first content mark)
        assert!(output.contains("from :1\n"));
    }
}
