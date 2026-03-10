//! Post-process converter output for the lore-git-md repository.
//!
//! Replaces the backtick-quoted Message-ID with a lore.kernel.org link
//! and inserts (or replaces) a thread link below the header table.

const THREAD_PREFIX: &str = "**Thread**: ";

/// Patch the markdown output from `email_to_markdown`:
///
/// 1. Replace `` `<message-id>` `` in the header table with a lore link
/// 2. Insert or replace the thread link line after the `---` separator
///
/// `email_dk` is the date-key of the email being patched.
/// `thread_root_key` is the date-key of the thread root (may be self).
/// `message_id` is the bare Message-ID (no angle brackets).
pub fn patch_markdown(
    markdown: &str,
    message_id: &str,
    email_dk: &str,
    thread_root_key: &str,
) -> String {
    let lore_link = format!("[{message_id}](https://lore.kernel.org/git/{message_id})");
    let backtick_id = format!("`{message_id}`");

    let email_dir = email_dk.rsplit_once('/').map_or("", |(d, _)| d);
    let thread_path = format!("{thread_root_key}.thread.md");
    let rel_path = crate::symlink::compute_relative_path(email_dir, &thread_path);
    let thread_line = format!("{THREAD_PREFIX}[thread]({rel_path}#:~:text={email_dk})");

    let mut result = String::with_capacity(markdown.len() + 200);
    let has_thread_link = markdown.lines().any(|l| l.starts_with(THREAD_PREFIX));
    let mut replaced_thread_link = false;

    for line in markdown.lines() {
        // Replace existing thread link
        if line.starts_with(THREAD_PREFIX) {
            result.push_str(&thread_line);
            result.push('\n');
            replaced_thread_link = true;
            continue;
        }

        if line.contains(&backtick_id) {
            result.push_str(&line.replace(&backtick_id, &lore_link));
        } else if !has_thread_link && !replaced_thread_link && line == "---" {
            result.push_str(line);
            result.push('\n');
            result.push('\n');
            result.push_str(&thread_line);
            replaced_thread_link = true;
            result.push('\n');
            continue;
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_EMAIL_MD: &str = concat!(
        "# [PATCH] Fix race condition in ref update\n",
        "\n",
        "| Header | Value |\n",
        "|--------|-------|\n",
        "| **From** | Alice Developer <alice@example.com> |\n",
        "| **To** | git@vger.kernel.org |\n",
        "| **Date** | 2025-02-12T09:40:17+05:30 |\n",
        "| **Message-ID** | `20250212041017.91370-1-alice@example.com` |\n",
        "\n",
        "---\n",
        "\n",
        "When updating refs concurrently, a TOCTOU race can occur.\n",
    );

    #[test]
    fn test_patch_message_id_link() {
        let result = patch_markdown(
            FULL_EMAIL_MD,
            "20250212041017.91370-1-alice@example.com",
            "2025/02/12/04-10-17",
            "2025/02/12/04-10-17",
        );
        assert!(result.contains("[20250212041017.91370-1-alice@example.com](https://lore.kernel.org/git/20250212041017.91370-1-alice@example.com)"));
        assert!(!result.contains("`20250212041017.91370-1-alice@example.com`"));
    }

    #[test]
    fn test_patch_thread_link_inserted() {
        let result = patch_markdown(
            FULL_EMAIL_MD,
            "20250212041017.91370-1-alice@example.com",
            "2025/02/12/04-10-17",
            "2025/02/12/04-10-17",
        );
        assert!(result.contains("**Thread**: [thread](04-10-17.thread.md"));
        let idx_sep = result.find("---").unwrap();
        let idx_thread = result.find("**Thread**").unwrap();
        let idx_body = result.find("When updating refs").unwrap();
        assert!(idx_thread > idx_sep);
        assert!(idx_body > idx_thread);
    }

    #[test]
    fn test_patch_thread_link_replaced() {
        // Simulate an already-patched email whose thread root changed
        let already_patched = concat!(
            "# [PATCH] Fix race condition in ref update\n",
            "\n",
            "| Header | Value |\n",
            "|--------|-------|\n",
            "| **From** | Alice Developer <alice@example.com> |\n",
            "| **To** | git@vger.kernel.org |\n",
            "| **Date** | 2025-02-12T09:40:17+05:30 |\n",
            "| **Message-ID** | [20250212041017.91370-1-alice@example.com]",
            "(https://lore.kernel.org/git/20250212041017.91370-1-alice@example.com) |\n",
            "\n",
            "---\n",
            "\n",
            "**Thread**: [thread](2025/01/01/00-00-00.thread.md#:~:text=2025/01/01/00-00-00)\n",
            "\n",
            "When updating refs concurrently, a TOCTOU race can occur.\n",
        );
        let result = patch_markdown(
            already_patched,
            "20250212041017.91370-1-alice@example.com",
            "2025/02/12/04-10-17",
            "2025/02/12/04-10-17",
        );
        // Old thread root should be gone
        assert!(!result.contains("2025/01/01/00-00-00"));
        // New thread root should be present
        assert!(result.contains("**Thread**: [thread](04-10-17.thread.md"));
        // Should appear exactly once
        assert_eq!(result.matches("**Thread**").count(), 1);
    }

    #[test]
    fn test_patch_preserves_body() {
        let result = patch_markdown(
            FULL_EMAIL_MD,
            "20250212041017.91370-1-alice@example.com",
            "2025/02/12/04-10-17",
            "2025/02/12/04-10-17",
        );
        assert!(result.contains("# [PATCH] Fix race condition in ref update\n"));
        assert!(result.contains("| **From** | Alice Developer <alice@example.com> |"));
        assert!(result.contains("When updating refs concurrently"));
    }

    #[test]
    fn test_thread_link_cross_directory() {
        let result = patch_markdown(
            FULL_EMAIL_MD,
            "20250212041017.91370-1-alice@example.com",
            "2025/02/13/10-00-00",
            "2025/02/12/04-10-17",
        );
        assert!(result.contains(
            "**Thread**: [thread](../12/04-10-17.thread.md#:~:text=2025/02/13/10-00-00)"
        ));
    }
}
