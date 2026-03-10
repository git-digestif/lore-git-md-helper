//! Batch-process source emails into structured results ready for import.
//!
//! Takes raw emails from `source_reader` and produces date-keyed markdown
//! files, thread trees, and an updated Message-ID map — everything needed
//! to generate a fast-import stream.

use std::collections::{HashMap, HashSet};

use mail_parser::MessageParser;

use crate::cat_file::BlobRead;
use crate::datekey::date_to_key_from_timestamp;
use crate::email_to_markdown;
use crate::lore_link::patch_markdown;
use crate::msgid_map::{MsgIdEntry, MsgIdMap};
use crate::source_reader::SourceEmail;
use crate::thread::{get_references, resolve_thread_root};
use crate::thread_file::{self, ThreadTree};

/// Result of processing a single email.
pub struct ProcessedEmail {
    pub date_key: String,
    pub markdown: String,
    pub thread_root: String,
}

/// Result of processing a batch of emails.
pub struct BatchResult {
    pub emails: Vec<ProcessedEmail>,
    pub trees: HashMap<String, ThreadTree>,
    pub skipped: u64,
    /// The source commit OID of the last email in this batch (for resumption).
    pub last_source_commit: Option<String>,
}

/// Process a slice of source emails into structured import data.
///
/// Thread trees are loaded on demand from the target repo (following
/// symlinks) when the batch encounters a thread that already exists.
/// `target_cat` is a persistent blob reader (typically CatFile) for loading
/// existing thread files; used for lazy thread root resolution.
pub fn process_emails(
    emails: &[SourceEmail],
    map: &mut MsgIdMap,
    existing_keys: &mut HashSet<String>,
    target_cat: &mut impl BlobRead,
) -> BatchResult {
    let parser = MessageParser::default();
    let mut results = Vec::new();
    let mut skipped = 0u64;
    let mut trees: HashMap<String, ThreadTree> = HashMap::new();
    // Reverse map: any date-key → its thread root date-key.
    let mut dk_to_root: HashMap<String, String> = HashMap::new();

    for src in emails {
        let msg = match parser.parse(&src.raw_email) {
            Some(m) => m,
            None => {
                eprintln!("WARN: unparseable email in {}, skipping", src.commit_oid);
                skipped += 1;
                continue;
            }
        };

        let msgid = match msg.message_id() {
            Some(id) => id.to_string(),
            None => {
                eprintln!("WARN: no Message-ID in {}, skipping", src.commit_oid);
                skipped += 1;
                continue;
            }
        };

        let ts = match msg.date() {
            Some(dt) => dt.to_timestamp(),
            None => {
                eprintln!("WARN: no Date in {}, skipping", src.commit_oid);
                skipped += 1;
                continue;
            }
        };

        let dk = match date_to_key_from_timestamp(ts, existing_keys) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("WARN: date conversion failed for {}: {e}", src.commit_oid);
                skipped += 1;
                continue;
            }
        };

        let subject = msg.subject().unwrap_or("(No Subject)").to_string();
        let from = msg
            .from()
            .and_then(|a| a.first())
            .map(|a| a.name().unwrap_or("?").to_string())
            .unwrap_or_else(|| "?".into());

        let old = map.insert_known(&msgid, dk.clone());

        let refs = get_references(&msg);
        let resolved_root = resolve_thread_root(&refs, &dk, map).unwrap_or_else(|| dk.clone());

        // If the resolved root is already a member of an existing thread,
        // follow to the actual root (avoids symlink chains).
        let thread_root = if let Some(root) = dk_to_root.get(&resolved_root) {
            root.clone()
        } else if let Some((root_dk, tree)) =
            thread_file::load_from_repo(target_cat, "refs/heads/main", &resolved_root)
        {
            for dk_in_tree in tree.date_keys() {
                dk_to_root.insert(dk_in_tree.to_string(), root_dk.clone());
            }
            trees.insert(root_dk.clone(), tree);
            root_dk
        } else {
            resolved_root
        };

        let tree = if !trees.contains_key(&thread_root) {
            let loaded = thread_file::load_from_repo(target_cat, "refs/heads/main", &thread_root)
                .map(|(_, t)| t)
                .unwrap_or_default();
            trees.entry(thread_root.clone()).or_insert(loaded)
        } else {
            trees.get_mut(&thread_root).unwrap()
        };

        // Find closest known ancestor already in the tree for placement
        let parent_dk = refs
            .iter()
            .rev()
            .filter_map(|r| match map.get(r) {
                MsgIdEntry::Known(k) => Some(k.clone()),
                _ => None,
            })
            .find(|k| tree.contains(k));

        tree.insert(&dk, parent_dk.as_deref(), &subject, &from);
        dk_to_root.insert(dk.clone(), thread_root.clone());

        // Reparent emails that were waiting for this message
        if let Some(MsgIdEntry::WantedBy(waiters)) = old {
            let tree = trees.get_mut(&thread_root).unwrap();
            for waiter_dk in &waiters {
                if tree.contains(waiter_dk) {
                    tree.reparent(waiter_dk, &dk);
                }
            }
        }

        let md = email_to_markdown(&msg).unwrap_or_else(|e| {
            eprintln!("WARN: markdown conversion failed for {msgid}: {e}");
            format!("# Error\n\nFailed to convert email: {e}\n")
        });
        let md = patch_markdown(&md, &msgid, &dk, &thread_root);

        results.push(ProcessedEmail {
            date_key: dk,
            markdown: md,
            thread_root,
        });
    }

    BatchResult {
        emails: results,
        trees,
        skipped,
        last_source_commit: emails.last().map(|e| e.commit_oid.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cat_file::MockBlobs;

    fn make_email(msgid: &str, date: &str, subject: &str, refs: &str) -> SourceEmail {
        let mut headers = format!(
            concat!(
                "From: Test <test@example.com>\r\n",
                "To: git@vger.kernel.org\r\n",
                "Subject: {}\r\n",
                "Date: {}\r\n",
                "Message-ID: <{}>\r\n",
            ),
            subject, date, msgid,
        );
        if !refs.is_empty() {
            headers.push_str(&format!("References: {refs}\r\n"));
        }
        headers.push_str("\r\nBody text.\r\n");
        SourceEmail {
            commit_oid: format!("fake-oid-{msgid}"),
            raw_email: headers.into_bytes(),
        }
    }

    #[test]
    fn test_single_email_produces_one_result() {
        let emails = [make_email(
            "a@test.com",
            "Mon, 10 Feb 2025 00:00:00 +0000",
            "[PATCH] Fix thing",
            "",
        )];
        let mut map = MsgIdMap::new(None);
        let mut keys = HashSet::new();
        let mut blobs = MockBlobs(std::collections::HashMap::new());

        let result = process_emails(&emails, &mut map, &mut keys, &mut blobs);

        assert_eq!(result.emails.len(), 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.emails[0].date_key, "2025/02/10/00-00-00");
        assert_eq!(result.emails[0].thread_root, "2025/02/10/00-00-00");
        assert!(result.emails[0].markdown.contains("[PATCH] Fix thing"));
    }

    #[test]
    fn test_reply_resolves_thread_root() {
        let emails = [
            make_email(
                "root@test.com",
                "Mon, 10 Feb 2025 00:00:00 +0000",
                "[PATCH 0/1] series",
                "",
            ),
            make_email(
                "reply@test.com",
                "Mon, 10 Feb 2025 01:00:00 +0000",
                "Re: [PATCH 0/1] series",
                "<root@test.com>",
            ),
        ];
        let mut map = MsgIdMap::new(None);
        let mut keys = HashSet::new();
        let mut blobs = MockBlobs(std::collections::HashMap::new());

        let result = process_emails(&emails, &mut map, &mut keys, &mut blobs);

        assert_eq!(result.emails.len(), 2);
        let root_dk = &result.emails[0].date_key;
        assert_eq!(&result.emails[1].thread_root, root_dk);
        assert!(result.trees.contains_key(root_dk));
    }

    #[test]
    fn test_unparseable_email_is_skipped() {
        let emails = [SourceEmail {
            commit_oid: "bad-oid".into(),
            raw_email: b"this is not a valid email".to_vec(),
        }];
        let mut map = MsgIdMap::new(None);
        let mut keys = HashSet::new();
        let mut blobs = MockBlobs(std::collections::HashMap::new());

        let result = process_emails(&emails, &mut map, &mut keys, &mut blobs);

        assert_eq!(result.emails.len(), 0);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_last_source_commit_tracks_final_email() {
        let emails = [
            make_email("a@test.com", "Mon, 10 Feb 2025 00:00:00 +0000", "first", ""),
            make_email(
                "b@test.com",
                "Mon, 10 Feb 2025 01:00:00 +0000",
                "second",
                "",
            ),
        ];
        let mut map = MsgIdMap::new(None);
        let mut keys = HashSet::new();
        let mut blobs = MockBlobs(std::collections::HashMap::new());

        let result = process_emails(&emails, &mut map, &mut keys, &mut blobs);

        assert_eq!(
            result.last_source_commit.as_deref(),
            Some("fake-oid-b@test.com"),
        );
    }
}
