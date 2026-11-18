//! How much attention a thread has drawn, counted from its thread tree.
//!
//! A long thread signals engagement and focus, so the digests report it
//! as a fact computed from the archive rather than as a model judgement.

use std::collections::HashSet;

use crate::date_util::{day_of, days_between};
use crate::thread_file::ThreadTree;

/// A thread with this many replies by the end of a day is long.
pub const LONG_REPLIES: usize = 20;

/// A thread with this many messages on one day is long on that day.
pub const LONG_MESSAGES_TODAY: usize = 8;

/// A thread's size as of the end of one day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadActivity {
    /// Messages whose subject starts with `Re:`.  Patch postings are not
    /// replies: a series of many patches is not a long discussion.
    pub replies: usize,
    /// Distinct senders.
    pub participants: usize,
    /// Messages sent on the day itself.  A patch series is sent as one
    /// email per patch, so the emails of one series count as one message.
    pub messages_today: usize,
    /// Days from the first message through the day, counting both.
    pub days: usize,
}

impl ThreadActivity {
    /// Count the thread's messages up to and including `day`
    /// ("YYYY/MM/DD"), or `None` if it has none by then.
    pub fn as_of(tree: &ThreadTree, day: &str) -> Option<Self> {
        let mut replies = 0;
        let mut senders = HashSet::new();
        let mut standalone_today = 0;
        let mut series_today = HashSet::new();
        let mut first: Option<&str> = None;
        for dk in tree.date_keys() {
            let message_day = day_of(dk);
            let Some(node) = tree.node_of(dk).filter(|_| message_day <= day) else {
                continue;
            };
            if is_reply(&node.subject) {
                replies += 1;
            }
            senders.insert(sender_name(&node.from));
            if message_day == day {
                match series_of(&node.subject, sender_name(&node.from)) {
                    Some(series) => {
                        series_today.insert(series);
                    }
                    None => standalone_today += 1,
                }
            }
            if first.is_none_or(|first| message_day < first) {
                first = Some(message_day);
            }
        }
        let days = usize::try_from(days_between(first?, day)? + 1).ok()?;
        Some(Self {
            replies,
            participants: senders.len(),
            messages_today: standalone_today + series_today.len(),
            days,
        })
    }

    pub fn is_long(&self) -> bool {
        self.replies >= LONG_REPLIES || self.messages_today >= LONG_MESSAGES_TODAY
    }
}

fn is_reply(subject: &str) -> bool {
    subject
        .trim_start()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
}

/// What identifies the emails of one patch series: their sender and the
/// leading tag of their subject, such as `[PATCH v3 5/7]`, without the
/// position in the series but with its length.  `None` for any other
/// message, including a reply to a patch.
fn series_of(subject: &str, sender: &str) -> Option<String> {
    let tag = subject.trim_start().strip_prefix('[')?.split_once(']')?.0;
    let (kind, position) = tag.rsplit_once(' ')?;
    let (number, total) = position.split_once('/')?;
    let is_number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let kind = kind.to_ascii_lowercase();
    (kind.contains("patch") && is_number(number) && is_number(total))
        .then(|| format!("{sender}\n{kind}\n{total}"))
}

/// The sender without the list a service relays them through, such as
/// "Julia Evans via GitGitGadget".
fn sender_name(from: &str) -> &str {
    from.split(" via ").next().unwrap_or(from).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_what_has_happened_by_the_day() {
        let mut tree = ThreadTree::new();
        let mut parent = None;
        for (dk, subject, from) in [
            (
                "2026/10/07/10-00-00",
                "[PATCH] doc: x",
                "Julia Evans via GitGitGadget",
            ),
            (
                "2026/10/07/11-00-00",
                "Re: [PATCH] doc: x",
                "Junio C Hamano",
            ),
            ("2026/10/09/12-00-00", "re: [PATCH] doc: x", "Julia Evans"),
            (
                "2026/10/09/13-00-00",
                "[PATCH v2] doc: x",
                "Julia Evans via GitGitGadget",
            ),
            ("2026/10/12/09-00-00", "Re: [PATCH v2] doc: x", "Ben Knoble"),
        ] {
            tree.insert(dk, parent, subject, from);
            parent = Some(dk);
        }

        assert_eq!(
            ThreadActivity::as_of(&tree, "2026/10/09"),
            Some(ThreadActivity {
                replies: 2,
                participants: 2,
                messages_today: 2,
                days: 3,
            }),
        );
        assert_eq!(ThreadActivity::as_of(&tree, "2026/10/06"), None);
    }

    #[test]
    fn a_thread_is_long_by_replies_or_by_one_busy_day() {
        let activity = |replies, messages_today| ThreadActivity {
            replies,
            participants: 2,
            messages_today,
            days: 1,
        };
        assert!(!activity(LONG_REPLIES - 1, LONG_MESSAGES_TODAY - 1).is_long());
        assert!(activity(LONG_REPLIES, 1).is_long());
        assert!(activity(3, LONG_MESSAGES_TODAY).is_long());
    }

    #[test]
    fn a_patch_series_counts_as_one_message() {
        let mut tree = ThreadTree::new();
        let mut parent = None;
        for (dk, subject, from) in [
            (
                "2026/10/07/10-00-00",
                "[PATCH v2 0/3] fetch: cover",
                "Julia Evans via GitGitGadget",
            ),
            (
                "2026/10/07/10-00-01",
                "[PATCH v2 1/3] fetch: one",
                "Julia Evans via GitGitGadget",
            ),
            (
                "2026/10/07/10-00-02",
                "[PATCH v2 2/3] fetch: two",
                "Julia Evans via GitGitGadget",
            ),
            (
                "2026/10/07/10-00-03",
                "[PATCH v2 3/3] fetch: three",
                "Julia Evans via GitGitGadget",
            ),
            // The same tag with another length is another series.
            (
                "2026/10/07/11-00-00",
                "[PATCH v2 1/2] doc: one",
                "Julia Evans",
            ),
            (
                "2026/10/07/11-00-01",
                "[PATCH v2 2/2] doc: two",
                "Julia Evans",
            ),
            // A lone patch and replies to patches are messages of their own.
            ("2026/10/07/12-00-00", "[PATCH] doc: lone", "Junio C Hamano"),
            (
                "2026/10/07/13-00-00",
                "Re: [PATCH v2 1/3] fetch: one",
                "Ben Knoble",
            ),
            (
                "2026/10/07/13-30-00",
                "Re: [PATCH v2 2/3] fetch: two",
                "Ben Knoble",
            ),
        ] {
            tree.insert(dk, parent, subject, from);
            parent = Some(dk);
        }

        let activity = ThreadActivity::as_of(&tree, "2026/10/07").unwrap();
        // Nine emails: two series, a lone patch and two replies.
        assert_eq!(activity.messages_today, 5);
        assert_eq!(activity.replies, 2);
        assert!(!activity.is_long());
    }
}
