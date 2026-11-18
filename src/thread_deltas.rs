//! A thread's cited daily deltas, kept from one day to the next.
//!
//! The daily digest extracts, for every thread active that day, one
//! cited bullet per development.  The ledger keeps those bullets in
//! `<root date-key>.thread.deltas.md`, so that a later digest can show
//! how a long thread got where it is without reading its whole history
//! again and without a summary of summaries that could drift.

use std::collections::BTreeMap;

use crate::cat_file::BlobRead;

/// The ledger file of the thread whose root has date-key `root_dk`.
pub fn path(root_dk: &str) -> String {
    format!("{root_dk}.thread.deltas.md")
}

/// A thread's bullets by day ("YYYY/MM/DD").  A day without bullets was
/// extracted and had nothing noteworthy; a day that is absent was not
/// extracted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThreadDeltas {
    days: BTreeMap<String, Vec<String>>,
}

impl ThreadDeltas {
    pub fn parse(md: &str) -> Self {
        let mut days: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in md.lines() {
            if let Some(day) = line.strip_prefix("## ") {
                let day = day.trim().to_string();
                days.entry(day.clone()).or_default();
                current = Some(day);
            } else if let (Some(day), Some(bullet)) = (&current, line.strip_prefix("- ")) {
                days.entry(day.clone())
                    .or_default()
                    .push(bullet.to_string());
            }
        }
        Self { days }
    }

    pub fn render(&self) -> String {
        let mut md = String::from("# Thread deltas\n");
        for (day, bullets) in &self.days {
            md.push_str(&format!("\n## {day}\n"));
            if !bullets.is_empty() {
                md.push('\n');
                for bullet in bullets {
                    md.push_str(&format!("- {bullet}\n"));
                }
            }
        }
        md
    }

    /// Set the bullets of `day`, replacing any recorded before.
    pub fn record(&mut self, day: &str, bullets: Vec<String>) {
        self.days.insert(day.to_string(), bullets);
    }

    /// The bullets recorded for `day`, or `None` if it was not extracted.
    pub fn day(&self, day: &str) -> Option<&[String]> {
        self.days.get(day).map(Vec::as_slice)
    }
}

/// The ledger files to commit so that every thread in `recorded` (root
/// date-key and its bullets for `day`) has `day` in its ledger, as
/// (path, content) pairs.  The days already in a ledger at `git_ref`
/// are kept.
pub fn record_day(
    day: &str,
    recorded: &[(String, Vec<String>)],
    git_ref: &str,
    cat: &mut impl BlobRead,
) -> Vec<(String, String)> {
    recorded
        .iter()
        .map(|(root_dk, bullets)| {
            let path = path(root_dk);
            let mut ledger = cat
                .get_str(&format!("{git_ref}:{path}"))
                .map(|md| ThreadDeltas::parse(&md))
                .unwrap_or_default();
            ledger.record(day, bullets.clone());
            (path, ledger.render())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cat_file::MockBlobs;

    fn ledger(days: &[(&str, &[&str])]) -> ThreadDeltas {
        let mut ledger = ThreadDeltas::default();
        for (day, bullets) in days {
            ledger.record(
                day,
                bullets.iter().map(|bullet| bullet.to_string()).collect(),
            );
        }
        ledger
    }

    #[test]
    fn a_rendered_ledger_parses_back_unchanged() {
        let original = ledger(&[
            (
                "2026/11/12",
                &[
                    "[2026/11/12/09-00-00 by Alice] Alice posts v1.",
                    "[2026/11/12/10-00-00 by Bob] Bob objects.",
                ],
            ),
            ("2026/11/13", &[]),
            (
                "2026/11/14",
                &["[2026/11/14/08-00-00 by Alice] Alice posts v2."],
            ),
        ]);

        assert_eq!(
            original.render(),
            concat!(
                "# Thread deltas\n",
                "\n",
                "## 2026/11/12\n",
                "\n",
                "- [2026/11/12/09-00-00 by Alice] Alice posts v1.\n",
                "- [2026/11/12/10-00-00 by Bob] Bob objects.\n",
                "\n",
                "## 2026/11/13\n",
                "\n",
                "## 2026/11/14\n",
                "\n",
                "- [2026/11/14/08-00-00 by Alice] Alice posts v2.\n",
            ),
        );
        assert_eq!(ThreadDeltas::parse(&original.render()), original);
    }

    #[test]
    fn recording_a_day_again_replaces_only_that_day() {
        let mut thread = ledger(&[("2026/11/12", &["first"]), ("2026/11/13", &["second"])]);
        thread.record("2026/11/12", vec!["redone".to_string()]);
        assert_eq!(
            thread,
            ledger(&[("2026/11/12", &["redone"]), ("2026/11/13", &["second"])]),
        );
    }

    #[test]
    fn record_day_keeps_the_days_already_in_a_ledger() {
        const ROOT: &str = "2026/10/07/10-00-00";
        const NEW_ROOT: &str = "2026/10/08/09-00-00";
        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(
            format!("main:{}", path(ROOT)),
            ledger(&[("2026/10/07", &["opened"])]).render(),
        );

        let files = record_day(
            "2026/10/08",
            &[
                (ROOT.to_string(), vec!["answered".to_string()]),
                (NEW_ROOT.to_string(), Vec::new()),
            ],
            "main",
            &mut blobs,
        );

        assert_eq!(
            files,
            vec![
                (
                    path(ROOT),
                    ledger(&[("2026/10/07", &["opened"]), ("2026/10/08", &["answered"])]).render(),
                ),
                (path(NEW_ROOT), ledger(&[("2026/10/08", &[])]).render()),
            ],
        );
    }
}
