//! Periodic (weekly/monthly) digest generation from daily digests.

use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;

use crate::ai_backend::Backend;
use crate::summarize::normalize_headings;

const PERIODIC_DIGEST_AGENT: &str = include_str!("../prompts/git-periodic-digest.md");

const PROJECT_CONTEXT: &str = include_str!("../prompts/git-project-context.md");

/// Granularity of the periodic digest.
#[derive(Clone, Copy)]
pub enum Granularity {
    Weekly,
    Monthly,
}

impl Granularity {
    pub fn label(self) -> &'static str {
        match self {
            Granularity::Weekly => "weekly",
            Granularity::Monthly => "monthly",
        }
    }

    /// The most threads that the leading `## Long threads` section of a
    /// digest may have; the prompt states the same numbers.
    fn long_thread_limit(self) -> usize {
        match self {
            Granularity::Weekly => 8,
            Granularity::Monthly => 10,
        }
    }
}

/// A single sub-period digest to include in the periodic rollup.
///
/// For weekly digests, this holds one day's daily digest.
/// For monthly digests, this holds one week's weekly digest.
pub struct SubDigest {
    pub label: String,
    pub content: String,
}

/// Build the user message for the periodic digest prompt.
pub fn build_periodic_input(
    period_label: &str,
    granularity: Granularity,
    digests: &[SubDigest],
) -> String {
    let mut msg = format!(
        "Period: {period_label}\nGranularity: {}\nSub-period digests: {}\n\n",
        granularity.label(),
        digests.len(),
    );

    for d in digests {
        msg.push_str("---\n\n");
        msg.push_str(&format!("Date: {}\n\n", d.label));
        msg.push_str(&d.content);
        msg.push_str("\n\n");
    }

    msg
}

/// Output from periodic digest generation.
pub struct PeriodicDigestOutput {
    pub human: String,
    pub ai: String,
}

/// The lines that mark a long thread, such as
/// `*Long thread: 77 replies so far from 6 participants over 56 days.*`.
fn long_thread_line() -> &'static Regex {
    static LINE: OnceLock<Regex> = OnceLock::new();
    LINE.get_or_init(|| Regex::new(r"\*Long thread[^*\n]*\*").unwrap())
}

fn long_thread_lines(md: &str) -> impl Iterator<Item = &str> {
    long_thread_line().find_iter(md).map(|found| found.as_str())
}

/// What a long-thread line states for a whole period: its text up to the
/// semicolon that introduces the figure for a single day.
fn long_thread_key(line: &str) -> &str {
    let text = line.trim_matches('*');
    text.split(';')
        .next()
        .unwrap_or(text)
        .trim()
        .trim_end_matches('.')
}

/// The long-thread lines of the sub-period digests, by what they state.
fn known_long_thread_keys(digests: &[SubDigest]) -> HashSet<&str> {
    digests
        .iter()
        .flat_map(|digest| long_thread_lines(&digest.content))
        .map(long_thread_key)
        .collect()
}

fn is_long_threads_heading(heading: &str) -> bool {
    heading
        .trim()
        .trim_matches(['*', '`', '#'])
        .trim()
        .trim_end_matches([':', '.'])
        .eq_ignore_ascii_case("long threads")
}

/// The number of subsections of the leading `## Long threads` section.
fn long_thread_count(digest: &str) -> usize {
    let mut lines = digest.lines().skip_while(|line| {
        !line
            .strip_prefix("## ")
            .is_some_and(is_long_threads_heading)
    });
    lines.next();
    lines
        .take_while(|line| !line.starts_with("## "))
        .filter(|line| line.starts_with("### "))
        .count()
}

/// What is wrong with the way a periodic digest treats long threads.
/// Their lines are code's figures, so the editor may only copy them from
/// a sub-period digest.  A digest leads with its long threads when the
/// sub-period digests mark any, has no such section otherwise, and
/// gives at most `limit` threads a subsection.
fn long_thread_issues(digest: &str, digests: &[SubDigest], limit: usize) -> Vec<String> {
    let known = known_long_thread_keys(digests);
    let mut issues: Vec<_> = long_thread_lines(digest)
        .filter(|line| !known.contains(long_thread_key(line)))
        .map(|line| {
            format!(
                "`{line}` is not a line of any sub-period digest; copy such \
                 lines exactly and never write one yourself"
            )
        })
        .collect();
    let mut headings = digest.lines().filter_map(|line| line.strip_prefix("## "));
    let leads = headings.clone().next().is_some_and(is_long_threads_heading);
    let has_section = headings.any(is_long_threads_heading);
    if !known.is_empty() && !leads {
        issues.push(
            "sub-period digests mark long threads, so the digest must open \
             with a `## Long threads` section"
                .to_string(),
        );
    }
    if known.is_empty() && has_section {
        issues.push(
            "no sub-period digest marks a long thread, so leave out the \
             `## Long threads` section"
                .to_string(),
        );
    }
    let threads = long_thread_count(digest);
    if threads > limit {
        issues.push(format!(
            "the `## Long threads` section has {threads} subsections; keep \
             the {limit} threads with the most replies and leave out the others"
        ));
    }
    issues
}

/// The digest without the long-thread lines that no sub-period digest
/// has, so that an invented figure is never published.
fn without_invented_long_thread_lines(digest: &str, digests: &[SubDigest]) -> String {
    static BLANKS: OnceLock<Regex> = OnceLock::new();
    let known = known_long_thread_keys(digests);
    let kept = long_thread_line().replace_all(digest, |found: &regex::Captures| {
        let text = found[0].to_string();
        if known.contains(long_thread_key(&text)) {
            text
        } else {
            String::new()
        }
    });
    let blanks = BLANKS.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    blanks.replace_all(&kept, "\n\n").into_owned()
}

/// One mode's digest.  A draft that does not treat long threads as
/// described above is written again once; if the second draft does not
/// either, it is published without the lines that were not copied.
async fn generate_mode(
    mode: &str,
    system: &str,
    user_msg: &str,
    digests: &[SubDigest],
    backend: &Backend,
    granularity: Granularity,
    clean: fn(&str) -> String,
) -> Result<String> {
    let label = granularity.label();
    let limit = granularity.long_thread_limit();
    let prompt = format!("Mode: {mode}\n\n{user_msg}");
    let chat = |prompt: String| async move {
        let draft = backend
            .chat_with_options(system, &prompt, Some(0.0))
            .await
            .with_context(|| format!("{label} digest ({mode}) failed"))?;
        anyhow::Ok(clean(&draft))
    };
    let draft = chat(prompt.clone()).await?;
    let issues = long_thread_issues(&draft, digests, limit);
    if issues.is_empty() {
        return Ok(draft);
    }
    eprintln!(
        "[periodic] {label} digest ({mode}) rejected: {}",
        issues.join("; ")
    );
    let findings: String = issues.iter().map(|issue| format!("- {issue}\n")).collect();
    let draft = chat(format!(
        "{prompt}\n\nYour previous draft was rejected:\n{findings}\
         Write the whole digest again and fix every point."
    ))
    .await?;
    let issues = long_thread_issues(&draft, digests, limit);
    if issues.is_empty() {
        return Ok(draft);
    }
    eprintln!(
        "[periodic] {label} digest ({mode}) rejected again: {}; publishing it \
         without the long-thread lines that no sub-period digest has",
        issues.join("; ")
    );
    Ok(without_invented_long_thread_lines(&draft, digests))
}

/// Generate a periodic digest from sub-period digests.
pub async fn generate_periodic_digest(
    period_label: &str,
    granularity: Granularity,
    digests: &[SubDigest],
    backend: &Backend,
) -> Result<PeriodicDigestOutput> {
    let system = format!("{PERIODIC_DIGEST_AGENT}\n\n{PROJECT_CONTEXT}");
    let user_msg = build_periodic_input(period_label, granularity, digests);

    eprintln!(
        "[periodic] generating {} digest for {period_label} ({} {} digests) ...",
        granularity.label(),
        digests.len(),
        match granularity {
            Granularity::Weekly => "daily",
            Granularity::Monthly => "weekly",
        },
    );

    let human = generate_mode(
        "human",
        &system,
        &user_msg,
        digests,
        backend,
        granularity,
        normalize_headings,
    )
    .await?;
    let ai = generate_mode(
        "ai",
        &system,
        &user_msg,
        digests,
        backend,
        granularity,
        str::to_string,
    )
    .await?;

    Ok(PeriodicDigestOutput { human, ai })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_periodic_input_structure() {
        let digests = vec![
            SubDigest {
                label: "2025/01/06".into(),
                content: "Monday digest".into(),
            },
            SubDigest {
                label: "2025/01/07".into(),
                content: "Tuesday digest".into(),
            },
        ];
        let input = build_periodic_input("2025/01/06 -- 2025/01/12", Granularity::Weekly, &digests);
        assert!(input.contains("Period: 2025/01/06 -- 2025/01/12"));
        assert!(input.contains("Granularity: weekly"));
        assert!(input.contains("Sub-period digests: 2"));
        assert!(input.contains("Date: 2025/01/06"));
        assert!(input.contains("Monday digest"));
        assert!(input.contains("Date: 2025/01/07"));
        assert!(input.contains("Tuesday digest"));
    }

    #[test]
    fn test_build_periodic_input_monthly() {
        let digests = vec![SubDigest {
            label: "2025/01/06 -- 2025/01/12".into(),
            content: "Week 1 summary".into(),
        }];
        let input = build_periodic_input("2025/01", Granularity::Monthly, &digests);
        assert!(input.contains("Granularity: monthly"));
        assert!(input.contains("Sub-period digests: 1"));
    }

    #[tokio::test]
    async fn test_generate_periodic_digest_with_mock() {
        let backend = Backend::Mock { nth_word: 3 };
        let digests = vec![
            SubDigest {
                label: "2025/01/06".into(),
                content: "alpha beta gamma delta epsilon".into(),
            },
            SubDigest {
                label: "2025/01/07".into(),
                content: "one two three four five six".into(),
            },
        ];
        let output = generate_periodic_digest(
            "2025/01/06 -- 2025/01/12",
            Granularity::Weekly,
            &digests,
            &backend,
        )
        .await
        .unwrap();
        assert!(!output.human.is_empty());
        assert!(!output.ai.is_empty());
    }

    const LONG_THREAD_LINE: &str =
        "*Long thread: 31 replies so far from 4 participants over 32 days.*";

    /// One daily digest that marks a thread as long.
    fn a_day_with_a_long_thread() -> Vec<SubDigest> {
        vec![SubDigest {
            label: "2025/01/06".into(),
            content: "## Long threads\n\n### fetch: shallow remotes\n\n\
                      *Long thread: 31 replies so far from 4 participants \
                      over 32 days; 1 message that day.*\n\nPosted v10.\n"
                .into(),
        }]
    }

    fn a_period_that_leads_with_it() -> String {
        format!(
            "## Long threads\n\n### fetch: shallow remotes\n\n{LONG_THREAD_LINE}\n\n\
             Posted v10.\n\n## The period in brief\n\nText.\n"
        )
    }

    #[test]
    fn test_long_thread_lines_are_copied_and_lead_the_digest() {
        let digests = a_day_with_a_long_thread();
        let limit = Granularity::Weekly.long_thread_limit();
        let led = a_period_that_leads_with_it();
        assert!(long_thread_issues(&led, &digests, limit).is_empty());

        // A figure the editor changed or wrote itself.
        let changed = led.replace("31 replies", "33 replies");
        let issues = long_thread_issues(&changed, &digests, limit);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("33 replies"));

        // The section has to come first.
        let late =
            format!("## The period in brief\n\nText.\n\n## Long threads\n\n{LONG_THREAD_LINE}\n");
        let issues = long_thread_issues(&late, &digests, limit);
        assert!(issues.iter().any(|issue| issue.contains("must open")));

        // Without a marked thread there is no such section.
        let quiet = vec![SubDigest {
            label: "2025/01/06".into(),
            content: "A quiet day.".into(),
        }];
        assert!(long_thread_issues("## The period in brief\n\nText.\n", &quiet, limit).is_empty());
        let issues = long_thread_issues(&led, &quiet, limit);
        assert!(issues.iter().any(|issue| issue.contains("leave out")));
    }

    /// Only the subsections of the leading section count, not the items
    /// of the sections after it.
    #[test]
    fn test_the_long_threads_section_has_at_most_as_many_threads_as_the_limit() {
        let digests = a_day_with_a_long_thread();
        let subsections: String = (0..3)
            .map(|n| format!("### thread {n}\n\n{LONG_THREAD_LINE}\n\nText.\n\n"))
            .collect();
        let digest = format!(
            "## Long threads\n\n{subsections}## Key developments\n\n### item one\n\nText.\n\n\
             ### item two\n\nText.\n"
        );
        assert!(long_thread_issues(&digest, &digests, 3).is_empty());
        let issues = long_thread_issues(&digest, &digests, 2);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("has 3 subsections"));
        assert!(issues[0].contains("keep the 2 threads"));
    }

    #[tokio::test]
    async fn test_a_digest_with_an_invented_line_is_written_again_then_cleaned() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let copied = a_period_that_leads_with_it();
        let invented = copied.replace("31 replies", "99 replies");
        let (copied_reply, invented_reply) = (copied.clone(), invented.clone());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let user = body["messages"][1]["content"].as_str().unwrap();
                // The human digest is right at the second try, the AI one never.
                let content = if user.starts_with("Mode: human") && user.contains("was rejected") {
                    &copied_reply
                } else {
                    &invented_reply
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": content}}]
                }))
            })
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };

        let output = generate_periodic_digest(
            "2025/01/06 -- 2025/01/12",
            Granularity::Weekly,
            &a_day_with_a_long_thread(),
            &backend,
        )
        .await
        .unwrap();

        assert!(output.human.contains(LONG_THREAD_LINE));
        assert!(!output.ai.contains("99 replies"));
        assert!(
            output
                .ai
                .contains("### fetch: shallow remotes\n\nPosted v10.")
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "each mode is asked twice");
    }
}
