//! Parser for Junio's "What's cooking in git.git" reports.
//!
//! Extracts each topic entry's section (`[New Topics]`, `[Stalled]`,
//! `[Cooking]`, `[Graduated to 'master']`, ...), short-name (e.g.
//! `tc/replay-linearize`), and one-line status (`Will merge to
//! 'next'.`, `Waiting for response(s) to review comment(s).`, ...).
//!
//! The parser is deterministic and used to override any hallucinated
//! merge-status text the LLM may have produced in a thread summary.
//! See `Digestive::commit_day_digest` for the daily-digest wiring.

use std::collections::HashMap;

/// A parsed entry from a "What's cooking" report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcTopic {
    /// Section header the entry sat under, without the brackets:
    /// `"New Topics"`, `"Stalled"`, `"Cooking"`, `"Graduated to 'master'"`.
    pub section: String,
    /// Junio's short-name for the topic: `"tc/replay-linearize"`.
    pub topic: String,
    /// The single-line status right before the `cf.`/`source:` block.
    /// Includes trailing punctuation as it appeared, e.g.
    /// `"Waiting for response(s) to review comment(s)."`.
    pub status_line: String,
}

/// Parse the body of a "What's cooking" email (already converted to
/// Markdown by `mbox2md`) into a map from source Message-ID to topic
/// entry.
///
/// The key is the raw Message-ID *without* the angle brackets,
/// matching the format `rag_parse::parse_email` returns in
/// `ParsedEmail::message_id`.  Only entries that carry a `source:`
/// line are returned; entries without one cannot be reconciled
/// against a thread and are silently dropped.
pub fn parse_whats_cooking(body: &str) -> HashMap<String, WcTopic> {
    let mut out: HashMap<String, WcTopic> = HashMap::new();
    let mut section: Option<String> = None;
    let mut cur: Option<PartialEntry> = None;

    for raw in body.lines() {
        let line = raw.trim_end();

        if is_section_separator(line) {
            finalize(cur.take(), &mut out);
            continue;
        }

        if let Some(name) = parse_section_header(line) {
            finalize(cur.take(), &mut out);
            section = Some(name.to_string());
            continue;
        }

        if let Some(topic) = parse_topic_header(line) {
            finalize(cur.take(), &mut out);
            cur = section.as_ref().map(|s| PartialEntry {
                section: s.clone(),
                topic: topic.to_string(),
                pending_status: None,
                source_msgid: None,
            });
            continue;
        }

        let Some(entry) = cur.as_mut() else { continue };

        // Skip mbox2md's inserted code-fence delimiters.
        if line == "```" {
            continue;
        }

        // Strip the single leading space that mbox2md preserves from
        // the original email indentation, so we see the topic body as
        // Junio wrote it.
        let inner = line.strip_prefix(' ').unwrap_or(line);
        let trimmed = inner.trim();

        if let Some(msgid) = strip_msgid(trimmed.strip_prefix("source:")) {
            entry.source_msgid = Some(msgid.to_string());
            continue;
        }
        if trimmed.starts_with("cf.") {
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with("- ")
            || trimmed.starts_with("+ ")
            || trimmed.starts_with('(')
        {
            continue;
        }

        entry.pending_status = Some(trimmed.to_string());
    }

    finalize(cur.take(), &mut out);
    out
}

struct PartialEntry {
    section: String,
    topic: String,
    pending_status: Option<String>,
    source_msgid: Option<String>,
}

fn finalize(entry: Option<PartialEntry>, out: &mut HashMap<String, WcTopic>) {
    let Some(e) = entry else { return };
    let (Some(msgid), Some(status)) = (e.source_msgid, e.pending_status) else {
        return;
    };
    out.insert(
        msgid,
        WcTopic {
            section: e.section,
            topic: e.topic,
            status_line: status,
        },
    );
}

/// Recognise `[Section Name]` on its own line and return the name
/// without brackets.
fn parse_section_header(line: &str) -> Option<&str> {
    let s = line.trim();
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() {
        return None;
    }
    Some(inner)
}

/// Recognise the `-----...` visual separator Junio inserts between
/// sections.  Any run of five or more `-` characters on its own
/// counts, so the current topic entry is closed before we cross
/// into the next section.
fn is_section_separator(line: &str) -> bool {
    let s = line.trim();
    s.len() >= 5 && s.chars().all(|c| c == '-')
}

/// Recognise `* short-name (YYYY-MM-DD) N commit(s)` and return the
/// short-name.  Requires the parenthesised date so we do not
/// accidentally match Markdown bullet lists elsewhere in the body.
fn parse_topic_header(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("* ")?;
    let (name, tail) = rest.split_once(' ')?;
    if !tail.starts_with('(') || !tail.contains(") ") {
        return None;
    }
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Extract a Message-ID from a `source:` value, stripping optional
/// angle brackets.  Returns `None` if the value is not present or
/// does not contain a `@`.
fn strip_msgid(rest: Option<&str>) -> Option<&str> {
    let v = rest?.trim();
    let inner = v
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(v);
    if inner.contains('@') {
        Some(inner)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag_parse;

    /// Verbatim excerpt from `2026/07/01/23-40-16.md` (Junio's
    /// "What's cooking in git.git (Jul 2026, #01)"), covering three
    /// distinct section/status combinations plus the fenced form the
    /// converter produces for already-merged entries.
    const FIXTURE: &str = "\
--------------------------------------------------
[New Topics]

* kk/commit-reach-find-all-fix (2026-06-29) 2 commits
 - commit-reach: guard !FIND_ALL early exit with generation ordering check
 - t6600: add test for merge-base early exit with clock skew

 The early-exit optimization in paint_down_to_common() has been gated
 on the queue being generation-ordered.

 Comments?
 cf. <xmqqa4sdw55v.fsf@gitster.g>
 source: <pull.2162.git.1782739162.gitgitgadget@gmail.com>


--------------------------------------------------
[Cooking]

* tc/replay-linearize (2026-06-25) 3 commits
 - replay: offer an option to linearize the commit topology
 - replay: better explain how pick_regular_commit() picks a base
 - replay: add helper to put entry into mapped_commits

 git replay learns --linearize option to drop merge commits and
 linearize the replayed history, mimicking git rebase
 --no-rebase-merges.

 Waiting for response(s) to review comment(s).
 cf. <xmqq5x358byf.fsf@gitster.g>
 source: <20260626-toon-git-replay-drop-merges-v5-0-5e120738b9d0@iotcl.com>


* ps/setup-drop-global-state (2026-06-10) 8 commits
```
  (merged to 'next' on 2026-06-15 at d9a8b88d47)
 + treewide: drop USE_THE_REPOSITORY_VARIABLE

 Continuation of \"setup.c\" refactoring to drop remaining global state.

 Will merge to 'master'.
 cf. <airVOrTboNDDGBak@denethor>
 source: <20260611-b4-pks-setup-drop-global-state-v2-0-a6f7269c841d@pks.im>

```
";

    #[test]
    fn extracts_linearize_topic_from_cooking_section() {
        let map = parse_whats_cooking(FIXTURE);
        let key = "20260626-toon-git-replay-drop-merges-v5-0-5e120738b9d0@iotcl.com";
        let t = map.get(key).unwrap_or_else(|| panic!("missing: {key}"));
        assert_eq!(t.section, "Cooking");
        assert_eq!(t.topic, "tc/replay-linearize");
        assert_eq!(
            t.status_line,
            "Waiting for response(s) to review comment(s).",
        );
    }

    #[test]
    fn extracts_new_topic_with_comments_status() {
        let map = parse_whats_cooking(FIXTURE);
        let t = map
            .get("pull.2162.git.1782739162.gitgitgadget@gmail.com")
            .expect("kk/commit-reach-find-all-fix");
        assert_eq!(t.section, "New Topics");
        assert_eq!(t.topic, "kk/commit-reach-find-all-fix");
        assert_eq!(t.status_line, "Comments?");
    }

    #[test]
    fn extracts_merged_to_next_entry_ignoring_annotation() {
        // The parenthesised `(merged to 'next' on ...)` line must
        // not be picked up as the status line; the actual status
        // ("Will merge to 'master'.") sits below the description.
        let map = parse_whats_cooking(FIXTURE);
        let t = map
            .get("20260611-b4-pks-setup-drop-global-state-v2-0-a6f7269c841d@pks.im")
            .expect("ps/setup-drop-global-state");
        assert_eq!(t.section, "Cooking");
        assert_eq!(t.status_line, "Will merge to 'master'.");
    }

    #[test]
    fn entries_without_source_are_dropped() {
        let body = "\
[New Topics]

* xx/no-source (2026-01-01) 1 commit
 - a commit subject

 Description here.

 Will merge to 'next'.
 cf. <xxx@example.com>
";
        assert!(parse_whats_cooking(body).is_empty());
    }

    #[test]
    fn empty_body_yields_empty_map() {
        assert!(parse_whats_cooking("").is_empty());
    }

    #[test]
    fn topic_header_requires_parenthesised_date() {
        // A bare Markdown bullet outside a topic entry must not be
        // mistaken for a topic header.
        assert!(parse_topic_header("* Some prose bullet, no date").is_none());
        assert!(parse_topic_header("* tc/replay-linearize (2026-06-25) 3 commits").is_some());
    }

    #[test]
    fn section_header_recognises_bracketed_labels() {
        assert_eq!(parse_section_header("[Cooking]"), Some("Cooking"));
        assert_eq!(
            parse_section_header("[Graduated to 'master']"),
            Some("Graduated to 'master'"),
        );
        assert_eq!(parse_section_header("Not a section"), None);
        assert_eq!(parse_section_header("[]"), None);
    }

    /// Full "What's cooking in git.git (Jul 2026, #01)" report,
    /// captured from `2026/07/01/23-40-16.md` in the corpus.  This
    /// is the exact input that, on 2026-07-01, coincided with a
    /// daily-digest LLM run publishing "git replay --linearize
    /// merged with post-merge issues identified" -- while Junio had
    /// in fact placed `tc/replay-linearize` in [Cooking], "Waiting
    /// for response(s) to review comment(s)."  The parser must
    /// extract that entry verbatim so downstream reconciliation
    /// leaves the LLM no room to make up a competing verdict.
    const REPORT_2026_07_01: &str = include_str!("../tests/fixtures/whats-cooking-2026-07-01.md");

    #[test]
    fn full_2026_07_01_report_reconciles_linearize_topic() {
        let body = rag_parse::parse_email(REPORT_2026_07_01).body;
        let map = parse_whats_cooking(&body);

        let linearize = map
            .get("20260626-toon-git-replay-drop-merges-v5-0-5e120738b9d0@iotcl.com")
            .expect("tc/replay-linearize must appear in the parsed map");
        assert_eq!(linearize.section, "Cooking");
        assert_eq!(linearize.topic, "tc/replay-linearize");
        assert_eq!(
            linearize.status_line, "Waiting for response(s) to review comment(s).",
            "the exact status line Junio wrote must be preserved verbatim",
        );

        // Guard against silent whole-section drops: the July 1
        // report has 70 topic entries across [New Topics] (8),
        // [Stalled] (4), and [Cooking] (58), all of which carry a
        // `source:` line and must survive parsing.
        assert_eq!(
            map.len(),
            70,
            "unexpected topic count -- a section may have been dropped",
        );
    }
}
