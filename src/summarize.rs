use anyhow::{Context, Result};

use crate::ai_backend::Backend;
use crate::rag_parse;

const EMAIL_AGENT: &str = include_str!("../prompts/git-digest-email.md");
const THREAD_AGENT: &str = include_str!("../prompts/git-thread-summary.md");
const PROJECT_CONTEXT: &str = include_str!("../prompts/git-project-context.md");

/// Maximum bytes of email Markdown to include in a summarization
/// prompt.  Picked to keep total prompt tokens well under the
/// default `DeepSeek-V3-0324` 128 k context window: 100 kB of email
/// body is roughly 25 k tokens, plus ~8 k for the system prompt and
/// a few k for thread/parent context = ~35-40 k total, comfortably
/// under the window.
///
/// Observed in practice on 2026-06-04: an 803 KB `[PATCH 6/6]` (a
/// mechanical sweep converting ~2800 bare `grep` calls to
/// `test_grep`) caused Azure OpenAI to return HTTP 200 with
/// `choices: []`, silently failing after charging ~221 k prompt
/// tokens against the 250 k-per-minute budget.  `x-ms-rai-invoked`
/// was `false` so this is a context-window overflow, not content
/// filtering.
pub const EMAIL_MD_BUDGET: usize = 100_000;

/// Build a mechanical "stub" pair of (human, ai) per-email summaries
/// for an email whose AI summarization was rejected (typically by a
/// backend content filter).  The pipeline writes these in place of
/// the regular `.summary.md` / `.ai.md` so the email still shows up
/// in downstream digests with at least its subject, author, and the
/// opening of its cover letter, instead of being silently invisible.
///
/// `reason` is the operator-facing one-line explanation of why the
/// stub was generated (e.g. `"Azure Responsible AI content filter"`);
/// it is woven into the human stub so a reader can tell at a glance
/// that the entry is not a real AI summary.
///
/// The opening of the cover letter is extracted as the first
/// paragraph after the `**Thread**:` line of the converted email
/// Markdown, capped at 1000 bytes so the stub never becomes huge on
/// long quote-laden replies; if no body is present the stub falls
/// back to a "(no cover letter text)" placeholder.
pub fn stub_summary_from_md(email_md: &str, reason: &str) -> (String, String) {
    let parsed = rag_parse::parse_email(email_md);
    let subject = if parsed.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        parsed.subject
    };
    let author = if parsed.author.is_empty() {
        "(unknown author)".to_string()
    } else {
        parsed.author
    };
    let body_excerpt = stub_body_excerpt(&parsed.body);

    let human = format!(
        "*[Mechanical stub: AI summarization was rejected by {reason}. \
         The full email content is available in the source repository.]*\n\
         \n\
         **{subject}** by {author}.\n\
         \n\
         {body_excerpt}\n"
    );
    let ai = format!(
        "Subject: {subject}\n\
         From: {author}\n\
         Note: AI summarization was rejected by {reason}; this entry is \
         a mechanical stub assembled from the email header and the opening \
         of the cover letter.\n\
         \n\
         Cover letter excerpt:\n\
         {body_excerpt}\n"
    );
    (human, ai)
}

/// Extract the first paragraph of the cover letter from a parsed
/// email body, capped at 1000 bytes (with a trailing ellipsis if
/// truncated).  Returns a placeholder if no body is present.
fn stub_body_excerpt(body: &str) -> String {
    const MAX: usize = 1000;
    let body = body.trim();
    if body.is_empty() {
        return "(no cover letter text)".to_string();
    }
    // Take everything up to the first blank line, which delimits the
    // first paragraph of the cover letter.  If the body is a single
    // paragraph, take the whole thing; the byte cap below catches
    // pathologically long ones.
    let para = body.split("\n\n").next().unwrap_or(body);
    if para.len() <= MAX {
        para.to_string()
    } else {
        // Cap on a char boundary so we never produce invalid UTF-8.
        let mut end = MAX;
        while end > 0 && !para.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &para[..end])
    }
}

pub struct EmailContext {
    pub email_md: String,
    pub thread_ai_summary: Option<String>,
    pub parent_ai_summary: Option<String>,
    /// Display name from the current email's `From:` header (address
    /// stripped).  Passed through to thread summarization as a
    /// "From: <Author>" header on the per-email brief so downstream
    /// LLMs never have to guess which participant made a given
    /// observation.
    pub from: Option<String>,
    /// Display name from the parent email's `From:` header (address
    /// stripped).  Used to annotate first-level quote blocks in
    /// `email_md` before summarization; the annotation is in-memory
    /// only and never written back to the corpus.
    pub parent_from: Option<String>,
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

/// If `s` exceeds `budget` bytes, return a truncated version that
/// keeps the header table, cover-letter prose, diffstat, and as
/// many whole `diff --git` file hunks as fit, followed by a
/// truncation marker.  Returns `None` if `s` already fits.
///
/// The cut is at the last `diff --git ` line whose start offset is
/// within `budget` when at least one such line fits, so we never
/// split a file diff in two; otherwise the cut is at the last
/// newline before `budget` so we never split any line.  Any fenced
/// code block left open by the cut is closed with a trailing
/// ```` ``` ```` so the prompt remains well-formed Markdown.
pub(crate) fn truncate_email_md(s: &str, budget: usize) -> Option<String> {
    if s.len() <= budget {
        return None;
    }

    // Walk every "\ndiff --git " boundary; record the last one whose
    // start offset still fits within `budget`.  Each boundary
    // represents the start of a file diff, so the count of boundaries
    // we passed equals the number of file diffs *plus one* (the one
    // we will not keep because it sits at or after the cut point).
    let needle = "\ndiff --git ";
    let mut cut_at: Option<usize> = None;
    let mut boundaries_seen: usize = 0;
    let mut search_from = 0;
    while let Some(off) = s[search_from..].find(needle) {
        let abs_line = search_from + off + 1;
        if abs_line > budget {
            break;
        }
        cut_at = Some(abs_line);
        boundaries_seen += 1;
        search_from = abs_line + needle.len() - 1;
    }
    let hunks_kept = boundaries_seen.saturating_sub(1);

    let cut_at = match cut_at {
        Some(pos) => pos,
        None => {
            // No "diff --git" boundary fits.  Cut at the last
            // newline before `budget` so we never split a line.
            s[..budget.min(s.len())]
                .rfind('\n')
                .map(|p| p + 1)
                .unwrap_or(0)
        }
    };

    let mut truncated = s[..cut_at].to_string();
    while truncated.ends_with('\n') {
        truncated.pop();
    }

    // Balance fences: an odd number of lines starting with ``` means
    // we cut inside an open code block; close it so the Markdown
    // around the truncation marker stays well-formed.
    let fence_lines = truncated
        .split('\n')
        .filter(|l| l.starts_with("```"))
        .count();
    if fence_lines % 2 == 1 {
        truncated.push_str("\n```");
    }

    let original_lines = s.lines().count();
    truncated.push_str(&format!(
        "\n\n*[Email body truncated for summarization: original was \
         {} bytes / {} lines; only the header, cover letter, diffstat, \
         and the first {} file diff(s) are shown.]*\n",
        s.len(),
        original_lines,
        hunks_kept,
    ));
    Some(truncated)
}

/// Build the user message for email summarization.
///
/// Assembles the thread context, parent context, and email body into
/// the format expected by the email digest prompt.  The email body
/// is truncated to `EMAIL_MD_BUDGET` if it exceeds it, so that the
/// total prompt fits comfortably in the model's context window.
pub fn email_user_message(ctx: &EmailContext, mode: &str) -> String {
    let mut msg = format!("Mode: {mode}\n\n");
    if let Some(thread) = &ctx.thread_ai_summary {
        msg.push_str("Thread AI summary:\n\n");
        msg.push_str(thread);
        msg.push_str("\n\n---\n\n");
    }
    if let Some(parent) = &ctx.parent_ai_summary {
        msg.push_str("Parent email AI summary:\n\n");
        msg.push_str(parent);
        msg.push_str("\n\n---\n\n");
    }
    msg.push_str("Email:\n\n");
    // Annotate first-level quotes with the parent's From when known,
    // so the summarizer never has to guess who wrote the quoted text.
    // In-memory only: the corpus markdown is untouched.
    let email_md_owned;
    let email_md_ref: &str = match ctx.parent_from.as_deref() {
        Some(pf) => {
            email_md_owned =
                crate::annotate::annotate_attribution(&ctx.email_md, ctx.from.as_deref(), Some(pf));
            &email_md_owned
        }
        None => &ctx.email_md,
    };
    match truncate_email_md(email_md_ref, EMAIL_MD_BUDGET) {
        Some(t) => {
            eprintln!(
                "[summarize] email body truncated from {} to {} bytes \
                 for prompt (budget {} bytes)",
                email_md_ref.len(),
                t.len(),
                EMAIL_MD_BUDGET,
            );
            msg.push_str(&t);
        }
        None => msg.push_str(email_md_ref),
    }
    msg
}

/// Build the system prompt for thread summarization.
pub fn thread_system_prompt() -> String {
    format!("{THREAD_AGENT}\n\n{PROJECT_CONTEXT}")
}

/// Build the user message for thread summarization.
///
/// When `new_email_from` is provided, the per-email brief is
/// preceded by a `From: <Author>` line so the thread agent can
/// attribute observations unambiguously without having to infer
/// the author from prose.
pub fn thread_user_message(
    existing_thread_ai: Option<&str>,
    new_email_ai: &str,
    new_email_from: Option<&str>,
    mode: &str,
) -> String {
    let mut msg = format!("Mode: {mode}\n\n");
    if let Some(thread) = existing_thread_ai {
        msg.push_str("Existing AI thread summary:\n\n");
        msg.push_str(thread);
        msg.push_str("\n\n---\n\n");
    }
    msg.push_str("New email AI summary:\n\n");
    if let Some(from) = new_email_from {
        msg.push_str(&format!("From: {from}\n\n"));
    }
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
            &thread_user_message(
                ctx.thread_ai_summary.as_deref(),
                &ai_summary,
                ctx.from.as_deref(),
                "human",
            ),
        )
        .await
        .context("thread human summary failed")?;

    let thread_ai_summary = cfg
        .chat(
            &thread_system,
            &thread_user_message(
                ctx.thread_ai_summary.as_deref(),
                &ai_summary,
                ctx.from.as_deref(),
                "ai",
            ),
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
    // Pre-pass: fix headings with missing space ("##Foo" -> "## Foo").
    // Process line-by-line so we don't accidentally mangle non-heading
    // content that happens to start with `#` after a blank line.
    let md = fix_heading_spaces(md);

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
        // Bold line followed by body text on the next line: treat
        // as a subsection heading (### …).
        if let Some((label, rest)) = strip_bold(trimmed)
            && !rest.is_empty()
        {
            out.push(format!("### {label}"));
            out.push(rest.to_string());
            continue;
        }
        out.push(part.to_string());
    }

    out.join("\n\n")
}

/// Fix ATX headings that lack the required space after the `#` run.
/// E.g. `##Notable threads` -> `## Notable threads`.
fn fix_heading_spaces(md: &str) -> String {
    let mut result = String::with_capacity(md.len() + 16);
    for (i, line) in md.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let hashes = line.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            let rest = &line[hashes..];
            if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('#') {
                result.push_str(&line[..hashes]);
                result.push(' ');
                result.push_str(rest);
                continue;
            }
        }
        result.push_str(line);
    }
    // Preserve trailing newline if the original had one.
    if md.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
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
            parent_ai_summary: None,
            from: None,
            parent_from: None,
        }
    }

    fn ctx_with_thread() -> EmailContext {
        EmailContext {
            email_md: "# [PATCH v2] Fix the frobnitz\nAddresses review".into(),
            thread_ai_summary: Some("Thread discusses frobnitz fix".into()),
            parent_ai_summary: Some("Parent proposed the fix".into()),
            from: None,
            parent_from: None,
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
        assert!(msg.contains("Parent email AI summary:\n\nParent proposed the fix"));
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
        let msg = thread_user_message(None, "AI summary of email", None, "human");
        assert!(msg.starts_with("Mode: human\n\n"));
        assert!(!msg.contains("Existing AI thread summary:"));
        assert!(!msg.contains("From:"), "no from means no From: line");
        assert!(msg.contains("New email AI summary:\n\nAI summary of email"));
    }

    #[test]
    fn thread_user_message_with_existing_thread() {
        let msg = thread_user_message(
            Some("prior thread state"),
            "new email AI",
            Some("Weijie Yuan"),
            "ai",
        );
        assert!(msg.starts_with("Mode: ai\n\n"));
        assert!(msg.contains("Existing AI thread summary:\n\nprior thread state"));
        assert!(msg.contains("From: Weijie Yuan\n\nnew email AI"));
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
    fn truncate_under_budget_returns_none() {
        let s = "short body";
        assert!(truncate_email_md(s, 100).is_none());
    }

    fn realistic_email_md() -> String {
        // Matches the mbox2md output shape: H1 subject, header table,
        // `---` separator, **Thread**: line, cover-letter paragraph,
        // a second paragraph (which the excerpt must NOT include).
        concat!(
            "# [PATCH 2/9] setup: stop applying repository format twice\n",
            "\n",
            "| Header | Value |\n",
            "|--------|-------|\n",
            "| **From** | Patrick Steinhardt <ps@pks.im> |\n",
            "| **To** | git@vger.kernel.org |\n",
            "| **Date** | 2026-06-10T16:57:08+02:00 |\n",
            "| **Message-ID** | [20260610-x@pks.im](https://lore.kernel.org/git/20260610-x@pks.im) |\n",
            "\n",
            "---\n",
            "\n",
            "**Thread**: [thread](14-57-06.thread.md)\n",
            "\n",
            "When discovering the repository in setup.c we apply the final\n",
            "repository format multiple times.\n",
            "\n",
            "Second paragraph that should be excluded from the stub excerpt.\n",
        )
        .to_string()
    }

    #[test]
    fn stub_extracts_subject_author_and_cover_letter_first_paragraph() {
        let (human, ai) =
            stub_summary_from_md(&realistic_email_md(), "Azure Responsible AI content filter");
        // Subject and author must appear in both.
        for s in [&human, &ai] {
            assert!(
                s.contains("[PATCH 2/9] setup: stop applying repository format twice"),
                "subject missing: {s}"
            );
            assert!(s.contains("Patrick Steinhardt"), "author missing: {s}");
            assert!(
                s.contains("Azure Responsible AI content filter"),
                "reason missing: {s}"
            );
        }
        // Cover letter excerpt: first paragraph only.
        assert!(human.contains("When discovering the repository in setup.c"));
        assert!(ai.contains("When discovering the repository in setup.c"));
        assert!(
            !human.contains("Second paragraph that should be excluded"),
            "human stub must stop at first blank line"
        );
        assert!(
            !ai.contains("Second paragraph that should be excluded"),
            "ai stub must stop at first blank line"
        );
        // Human stub must clearly mark itself as a mechanical stub.
        assert!(human.starts_with("*[Mechanical stub:"));
    }

    #[test]
    fn stub_handles_missing_subject_and_author_gracefully() {
        let md = "Body but no subject heading and no metadata table.";
        let (human, ai) = stub_summary_from_md(md, "test reason");
        assert!(human.contains("(no subject)"), "{human}");
        assert!(human.contains("(unknown author)"), "{human}");
        // The body excerpt should still be present (parse_email falls
        // back to the line-skipping branch when there's no Thread:
        // marker), and the test reason should be threaded through.
        assert!(ai.contains("test reason"));
    }

    #[test]
    fn stub_handles_empty_cover_letter() {
        // Header-only email (cover letter is just the header table
        // with no body after the `---`).  parse_email returns an
        // empty body; the stub must not panic and must produce a
        // visible placeholder.
        let md =
            "# Subject\n\n| Header | Value |\n|--------|-------|\n| **From** | Alice |\n\n---\n";
        let (human, ai) = stub_summary_from_md(md, "test");
        assert!(human.contains("(no cover letter text)"), "{human}");
        assert!(ai.contains("(no cover letter text)"), "{ai}");
    }

    #[test]
    fn stub_excerpt_caps_pathologically_long_first_paragraph_on_char_boundary() {
        // A single 5000-byte first paragraph with a 4-byte UTF-8
        // character at byte 998: the cap (at 1000) must back off to
        // a char boundary so the result is valid UTF-8 and ends
        // with "...".
        let mut body = "a".repeat(998);
        body.push('\u{1F600}'); // 4-byte char starting at byte 998
        body.push_str(&"b".repeat(4000));
        let md = format!(
            "# S\n\n| Header | Value |\n|---|---|\n| **From** | X |\n\n---\n\n**Thread**: [t](t.md)\n\n{body}\n"
        );
        let (human, _ai) = stub_summary_from_md(&md, "test");
        assert!(
            human.ends_with("...\n") || human.contains("...\n"),
            "{human}"
        );
        // No panic = char-boundary cap worked; verify the chosen
        // truncation point is before the 4-byte char to keep the
        // body excerpt as long as possible without splitting it.
        let excerpt_start = human.find("**S** by X.").unwrap();
        let excerpt = &human[excerpt_start..];
        // Just assert no embedded U+1F600 (we cut before it).
        assert!(
            !excerpt.contains('\u{1F600}'),
            "should have cut before the 4-byte char"
        );
    }

    #[test]
    fn truncate_cuts_at_diff_git_boundary() {
        let head = "# Subject\n\n| Header | Value |\n|---|---|\n| **From** | A |\n\n---\n\nCover.\n\n```\n stats\n```\n\n```diff\n";
        let f1 = "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let f2 = "diff --git a/y b/y\n@@ -1 +1 @@\n-old\n+new\n";
        let f3 = "diff --git a/zzz b/zzz\n@@ -1 +1 @@\n-old\n+new\n";
        let s = format!("{head}{f1}{f2}{f3}```\n");
        // Budget set so that the boundary search records f3 as a fit
        // but cuts content right at its start, keeping f1 and f2 in
        // full but dropping f3.
        let budget = s.find("diff --git a/zzz").unwrap();
        let out = truncate_email_md(&s, budget).expect("should truncate");
        assert!(out.contains("a/x"));
        assert!(out.contains("a/y"));
        assert!(!out.contains("a/zzz"), "third file diff must be dropped");
        assert!(out.contains("first 2 file diff(s)"));
        assert!(
            out.matches("```").count().is_multiple_of(2),
            "fences must be balanced: {out}"
        );
    }

    #[test]
    fn truncate_cover_letter_only_falls_back_to_newline_cut() {
        let line = "This is a long cover letter line that repeats.\n";
        let s = line.repeat(50);
        let budget = 200;
        let out = truncate_email_md(&s, budget).expect("should truncate");
        assert!(out.contains("truncated for summarization"));
        assert!(out.contains("first 0 file diff(s)"));
        // The truncated content should end on a line boundary (never
        // split a line in two), and be under the original size.
        assert!(out.len() < s.len());
        let body_before_marker = out
            .split_once("\n\n*[Email body truncated")
            .map(|(b, _)| b)
            .unwrap_or(&out);
        assert!(
            body_before_marker.lines().all(|l| line.trim_end() == l),
            "every retained line should be intact: {body_before_marker:?}"
        );
    }

    #[test]
    fn truncate_closes_open_fence() {
        // The cut lands inside an open ```diff block: the function
        // must append a closing ``` so the prompt stays well-formed.
        let head = "Pre\n\n```diff\n";
        let f1 = "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let f2 = "diff --git a/y b/y\n@@ -1 +1 @@\n-old\n+new\n";
        let s = format!("{head}{f1}{f2}```\n");
        let budget = s.find("diff --git a/y").unwrap() - 1;
        let out = truncate_email_md(&s, budget).expect("should truncate");
        assert!(
            out.matches("```").count().is_multiple_of(2),
            "fences must balance after truncation: {out}"
        );
        // The closing fence must sit between the kept diff content
        // and the truncation marker, not after the marker.
        let close_pos = out.rfind("```").expect("closing fence present");
        let marker_pos = out
            .find("\n\n*[Email body truncated")
            .expect("marker present");
        assert!(
            close_pos < marker_pos,
            "closing fence must come before marker: {out}"
        );
    }

    #[test]
    fn email_user_message_truncates_oversized_body() {
        let line = "diff --git a/file b/file\n@@ -1 +1 @@\n-old\n+new\n";
        let big = line.repeat(5000);
        assert!(
            big.len() > EMAIL_MD_BUDGET,
            "sanity: test setup must exceed budget"
        );
        let ctx = EmailContext {
            email_md: big,
            thread_ai_summary: None,
            parent_ai_summary: None,
            from: None,
            parent_from: None,
        };
        let msg = email_user_message(&ctx, "human");
        assert!(
            msg.contains("Email body truncated for summarization"),
            "expected truncation marker"
        );
        // The assembled prompt body should not exceed the budget by more
        // than a small constant (mode header + marker).
        assert!(
            msg.len() < EMAIL_MD_BUDGET + 4096,
            "assembled message {} bytes should be ~budget, not full size",
            msg.len()
        );
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

    #[test]
    fn normalize_fixes_missing_space_after_hashes() {
        let input = "# Good title\n\n##Notable threads\n\nContent\n\n###Sub heading\n\nMore\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("## Notable threads"),
            "expected space after ##, got: {out}"
        );
        assert!(
            out.contains("### Sub heading"),
            "expected space after ###, got: {out}"
        );
    }

    #[test]
    fn normalize_bold_topic_to_h3() {
        let input = "## Notable threads\n\n**fsmonitor approved**  \nPaul's series gets merged.\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("### fsmonitor approved"),
            "expected ### heading for bold topic, got: {out}"
        );
        assert!(
            out.contains("Paul's series gets merged."),
            "content should be preserved, got: {out}"
        );
    }

    #[test]
    fn normalize_inline_bold_not_promoted() {
        let input = "## In brief\n\n**Reftable fix** -- Patrick fixes a bug.\n";
        let out = normalize_headings(input);
        assert!(
            out.contains("**Reftable fix** -- Patrick fixes a bug."),
            "inline bold should stay as-is, got: {out}"
        );
    }
}
