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
        human_summary,
        ai_summary,
        thread_human_summary,
        thread_ai_summary,
    })
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
}
