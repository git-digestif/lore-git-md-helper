//! Periodic (weekly/monthly) digest generation from daily digests.

use anyhow::{Context, Result};

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

    let human = backend
        .chat_with_options(&system, &format!("Mode: human\n\n{user_msg}"), Some(0.0))
        .await
        .with_context(|| format!("{} digest (human) failed", granularity.label()))?;

    let ai = backend
        .chat_with_options(&system, &format!("Mode: ai\n\n{user_msg}"), Some(0.0))
        .await
        .with_context(|| format!("{} digest (AI) failed", granularity.label()))?;

    Ok(PeriodicDigestOutput {
        human: normalize_headings(&human),
        ai,
    })
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
}
