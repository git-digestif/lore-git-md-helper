use anyhow::{Context, Result};

use crate::ai_backend::Backend;

const EMAIL_AGENT: &str = include_str!("../prompts/git-digest-email.md");
const THREAD_AGENT: &str = include_str!("../prompts/git-thread-summary.md");
const PROJECT_CONTEXT: &str = include_str!("../prompts/git-project-context.md");

pub struct EmailContext {
    pub email_md: String,
    pub thread_ai_summary: Option<String>,
}

pub struct SummarizationOutput {
    pub human_summary: String,
    pub ai_summary: String,
    pub thread_human_summary: String,
    pub thread_ai_summary: String,
}

/// Build the system prompt for email summarization.
pub fn email_system_prompt() -> String {
    format!("{EMAIL_AGENT}\n\n{PROJECT_CONTEXT}")
}

/// Build the user message for email summarization.
///
/// Assembles the thread context, parent context, and email body into
/// the format expected by the email digest prompt.
pub fn email_user_message(ctx: &EmailContext, mode: &str) -> String {
    let mut msg = format!("Mode: {mode}\n\n");
    if let Some(thread) = &ctx.thread_ai_summary {
        msg.push_str("Thread AI summary:\n\n");
        msg.push_str(thread);
        msg.push_str("\n\n---\n\n");
    }
    msg.push_str("Email:\n\n");
    msg.push_str(&ctx.email_md);
    msg
}

/// Build the system prompt for thread summarization.
pub fn thread_system_prompt() -> String {
    format!("{THREAD_AGENT}\n\n{PROJECT_CONTEXT}")
}

/// Build the user message for thread summarization.
pub fn thread_user_message(
    existing_thread_ai: Option<&str>,
    new_email_ai: &str,
    mode: &str,
) -> String {
    let mut msg = format!("Mode: {mode}\n\n");
    if let Some(thread) = existing_thread_ai {
        msg.push_str("Existing AI thread summary:\n\n");
        msg.push_str(thread);
        msg.push_str("\n\n---\n\n");
    }
    msg.push_str("New email AI summary:\n\n");
    msg.push_str(new_email_ai);
    msg
}

pub async fn summarize_email(ctx: &EmailContext, cfg: &Backend) -> Result<SummarizationOutput> {
    let email_system = email_system_prompt();

    let human_summary = cfg
        .chat(&email_system, &email_user_message(ctx, "human"))
        .await
        .context("human summary failed")?;

    let ai_summary = cfg
        .chat(&email_system, &email_user_message(ctx, "ai"))
        .await
        .context("AI summary failed")?;

    let thread_system = thread_system_prompt();

    let thread_human_summary = cfg
        .chat(
            &thread_system,
            &thread_user_message(ctx.thread_ai_summary.as_deref(), &ai_summary, "human"),
        )
        .await
        .context("thread human summary failed")?;

    let thread_ai_summary = cfg
        .chat(
            &thread_system,
            &thread_user_message(ctx.thread_ai_summary.as_deref(), &ai_summary, "ai"),
        )
        .await
        .context("thread AI summary failed")?;

    Ok(SummarizationOutput {
        human_summary: normalize_headings(&human_summary),
        ai_summary,
        thread_human_summary: normalize_headings(&thread_human_summary),
        thread_ai_summary,
    })
}

/// Section names that should be `## …` headings.
const SECTION_HEADINGS: &[&str] = &[
    "notable threads",
    "in brief",
    "the day in brief",
    "on the radar",
    "future directions",
    "looking ahead",
    "key developments",
];

/// Normalize AI-generated Markdown that uses bold text instead of
/// proper heading syntax.
///
/// Handles two separate concerns:
/// 1. The first paragraph: if it is a short plain-text or bold line
///    mentioning a month name or "digest", promote it to `# …`.
/// 2. Section headings: `**Notable threads**` etc. at the start of
///    a paragraph are promoted to `## …`, splitting off any
///    following content in the same paragraph.
pub fn normalize_headings(md: &str) -> String {
    let mut paragraphs = md.split("\n\n").peekable();
    let mut out: Vec<String> = Vec::new();

    // Handle the first paragraph separately: promote short
    // title-like text to `# …`.  Only consumed when it matches;
    // otherwise it falls through to the normal loop.
    if let Some(&first) = paragraphs.peek() {
        let t = first.trim();
        if !t.is_empty() && !t.starts_with('#') && is_title_line(t) {
            paragraphs.next();
            if let Some((inner, _)) = strip_bold(t) {
                out.push(format!("# {inner}"));
            } else {
                out.push(format!("# {t}"));
            }
        }
    }

    for part in paragraphs {
        let trimmed = part.trim();
        if let Some((label, rest)) = strip_bold(trimmed)
            && is_known_section(label)
        {
            out.push(format!("## {label}"));
            if !rest.is_empty() {
                out.push(rest.to_string());
            }
            continue;
        }
        out.push(part.to_string());
    }

    out.join("\n\n")
}

/// True if `text` is a short single-line string that looks like a
/// digest title (mentions a month name or the word "digest").
fn is_title_line(text: &str) -> bool {
    lazy_static_regex().is_match(text) && !text.contains('\n') && text.len() < 80
}

fn lazy_static_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(
        r"(?i)\b(?:january|february|march|april|may|june|july|august|september|october|november|december|digest)\b"
    ).unwrap())
}

/// If `text` starts with `**label**` (followed by optional
/// whitespace/punctuation and then end-of-string or newline),
/// return `(label, remaining_text)`.
fn strip_bold(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_prefix("**")?;
    let end = inner.find("**")?;
    let label = &inner[..end];
    let after = &inner[end + 2..];
    let after = after.trim_start_matches([' ', '\t', '.', ':']);
    if after.is_empty() {
        Some((label, ""))
    } else if let Some(rest) = after.strip_prefix('\n') {
        Some((label, rest))
    } else {
        None
    }
}

fn is_known_section(label: &str) -> bool {
    let normalized = label.trim_end_matches(['.', ':']).trim().to_lowercase();
    SECTION_HEADINGS.contains(&normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> EmailContext {
        EmailContext {
            email_md: "# [PATCH] Fix the frobnitz\nSigned-off-by: A".into(),
            thread_ai_summary: None,
        }
    }

    fn ctx_with_thread() -> EmailContext {
        EmailContext {
            email_md: "# [PATCH v2] Fix the frobnitz\nAddresses review".into(),
            thread_ai_summary: Some("Thread discusses frobnitz fix".into()),
        }
    }

    #[test]
    fn email_system_prompt_includes_agent_and_context() {
        let sys = email_system_prompt();
        assert!(
            sys.contains("Human digest mode"),
            "should contain human mode instructions from email agent"
        );
        assert!(
            sys.contains("Summarizer brief mode"),
            "should contain AI mode instructions"
        );
        assert!(sys.contains("Junio"), "should contain project context");
    }

    #[test]
    fn email_user_message_includes_email_body() {
        let ctx = sample_ctx();
        let msg = email_user_message(&ctx, "human");
        assert!(msg.starts_with("Mode: human\n\n"), "should start with mode");
        assert!(
            msg.contains("Fix the frobnitz"),
            "should contain email body"
        );
        assert!(msg.contains("Email:\n\n"), "should have Email: header");
        // No thread or parent context for a root email
        assert!(!msg.contains("Thread AI summary:"));
        assert!(!msg.contains("Parent email AI summary:"));
    }

    #[test]
    fn email_user_message_includes_thread_context() {
        let ctx = ctx_with_thread();
        let msg = email_user_message(&ctx, "ai");
        assert!(msg.starts_with("Mode: ai\n\n"));
        assert!(msg.contains("Thread AI summary:\n\nThread discusses frobnitz fix"));
        assert!(msg.contains("Email:\n\n# [PATCH v2]"));
    }

    #[test]
    fn thread_system_prompt_includes_agent_and_context() {
        let sys = thread_system_prompt();
        assert!(
            sys.contains("thread summary"),
            "should contain thread agent instructions"
        );
        assert!(sys.contains("Junio"), "should contain project context");
    }

    #[test]
    fn thread_user_message_without_existing_thread() {
        let msg = thread_user_message(None, "AI summary of email", "human");
        assert!(msg.starts_with("Mode: human\n\n"));
        assert!(!msg.contains("Existing AI thread summary:"));
        assert!(msg.contains("New email AI summary:\n\nAI summary of email"));
    }

    #[test]
    fn thread_user_message_with_existing_thread() {
        let msg = thread_user_message(Some("prior thread state"), "new email AI", "ai");
        assert!(msg.starts_with("Mode: ai\n\n"));
        assert!(msg.contains("Existing AI thread summary:\n\nprior thread state"));
        assert!(msg.contains("New email AI summary:\n\nnew email AI"));
    }

    #[test]
    fn user_message_size_sanity() {
        // With a typical email, the assembled user message should be
        // reasonably sized (not accidentally including the system
        // prompt or duplicating content).
        let ctx = ctx_with_thread();
        let msg = email_user_message(&ctx, "human");
        assert!(
            msg.len() < 1000,
            "assembled message for a short email should be compact, got {} bytes",
            msg.len()
        );
        assert!(msg.len() > 50, "message should not be empty");

        // The system prompt is much larger (includes full project context)
        let sys = email_system_prompt();
        assert!(
            sys.len() > msg.len(),
            "system prompt should be larger than user message for short emails"
        );
    }

    #[test]
    fn normalize_bold_section_to_h2() {
        let input = "Some intro\n\n**Notable threads**\n\nContent here\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("## Notable threads"),
            "expected ## heading, got: {out}"
        );
        assert!(!out.contains("**Notable threads**"));
    }

    #[test]
    fn normalize_bold_title_with_month() {
        let input = "**Daily digest for March 11, 2026**\n\nBody\n";
        let out = normalize_headings(input);
        assert!(
            out.starts_with("# Daily digest for March 11, 2026"),
            "expected # heading, got: {out}"
        );
    }

    #[test]
    fn normalize_plain_title_with_month() {
        let input = "Here's the daily digest for March 11, 2026:\n\nBody\n";
        let out = normalize_headings(input);
        assert!(
            out.starts_with("# Here's the daily digest for March 11, 2026:"),
            "expected # heading, got: {out}"
        );
    }

    #[test]
    fn normalize_preserves_proper_headings() {
        let input = "# Good title\n\n## Notable threads\n\nContent\n";
        assert_eq!(normalize_headings(input), input);
    }

    #[test]
    fn normalize_bold_with_trailing_colon() {
        let input = "**In brief**:\n\nStuff\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("## In brief"),
            "expected ## heading, got: {out}"
        );
    }

    #[test]
    fn normalize_bold_with_inner_punctuation() {
        let input = "**The day in brief.**\n\nContent\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("## The day in brief."),
            "expected ## heading, got: {out}"
        );
    }

    #[test]
    fn normalize_ignores_bold_in_paragraph() {
        let input = "This has **in brief** inside a sentence.\n";
        assert_eq!(normalize_headings(input), input);
    }

    #[test]
    fn normalize_splits_heading_from_fused_content() {
        let input = "**Notable threads**\ncontinued on this line.\n";
        let out = normalize_headings(input);
        assert!(
            out.starts_with("## Notable threads\n\n"),
            "heading should be split from content, got: {out}"
        );
        assert!(
            out.contains("continued on this line."),
            "content should be preserved"
        );
    }

    #[test]
    fn normalize_fused_section_with_hard_break() {
        // Real-world pattern: heading with trailing spaces (hard
        // break) fused with content in the same paragraph.
        let input = "**In brief**  \n**Upload-pack series** -- details here.\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("## In brief"),
            "expected ## heading, got: {out}"
        );
        assert!(
            out.contains("**Upload-pack series**"),
            "content should be preserved"
        );
    }

    #[test]
    fn normalize_long_first_line_not_promoted() {
        let input = "This is a much longer introductory paragraph that happens to mention January but should not become a heading because it is too long.\n";
        assert_eq!(normalize_headings(input), input);
    }
}
