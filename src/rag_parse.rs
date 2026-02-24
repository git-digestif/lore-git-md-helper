use regex::Regex;
use std::sync::OnceLock;

pub struct ParsedEmail {
    pub subject: String,
    pub author: String,
    pub date: String,
    pub message_id: String,
    pub body: String,
}

/// Strip markdown links `[text](url)` → `text`.
fn strip_links(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());
    re.replace_all(s, "$1").into_owned()
}

/// Parse a converted markdown email into its structured fields.
///
/// The expected format is the output of `mbox2md`:
/// a `# Subject` heading, a metadata table with `**Header**` rows,
/// a `**Thread**:` line, then the body text.
pub fn parse_email(src: &str) -> ParsedEmail {
    static RE_SUBJECT: OnceLock<Regex> = OnceLock::new();
    static RE_META: OnceLock<Regex> = OnceLock::new();
    static RE_THREAD: OnceLock<Regex> = OnceLock::new();

    let re_subject = RE_SUBJECT.get_or_init(|| Regex::new(r"(?m)^#\s+(.+?)$").unwrap());
    let re_meta =
        RE_META.get_or_init(|| Regex::new(r"\*\*(\w[\w-]*)\*\*\s*\|\s*(.+?)\s*\|").unwrap());
    let re_thread = RE_THREAD.get_or_init(|| Regex::new(r"\*\*Thread\*\*:.*?\)").unwrap());

    let subject = re_subject
        .captures(src)
        .map(|c| strip_links(c[1].trim()))
        .unwrap_or_default();

    let mut author = String::new();
    let mut date = String::new();
    let mut message_id = String::new();

    for cap in re_meta.captures_iter(src) {
        let key = cap[1].to_lowercase();
        let val = strip_links(cap[2].trim());
        match key.as_str() {
            "from" => author = val,
            "date" => date = val,
            "message-id" => message_id = val,
            _ => {}
        }
    }

    // Body: everything after the **Thread**: … line.
    // Fallback: skip heading/table/rule lines and take what remains.
    let body = if let Some(m) = re_thread.find(src) {
        src[m.end()..].trim_start().to_owned()
    } else {
        src.lines()
            .skip_while(|l| {
                let t = l.trim();
                t.starts_with('#') || t.starts_with('|') || t.starts_with("---") || t.is_empty()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    ParsedEmail {
        subject,
        author,
        date,
        message_id,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "# [PATCH] object-name: fix resolution of object names containing curly braces\n",
        "\n",
        "| Header | Value |\n",
        "|--------|-------|\n",
        "| **From** | Elijah Newren via GitGitGadget <gitgitgadget@gmail.com> |\n",
        "| **To** | git@vger.kernel.org |\n",
        "| **Date** | 2025-01-01T02:53:09Z |\n",
        "| **Message-ID** | [pull.1844.git.1735699989371.gitgitgadget@gmail.com](https://lore.kernel.org/git/pull.1844.git.1735699989371.gitgitgadget@gmail.com) |\n",
        "\n",
        "---\n",
        "\n",
        "**Thread**: [thread](./02-53-09.thread.md#:~:text=2025/01/01/02-53-09)\n",
        "\n",
        "From: Elijah Newren <newren@gmail.com>\n",
        "\n",
        "Given a branch name of 'foo{bar', commands like\n",
    );

    #[test]
    fn parse_subject() {
        let e = parse_email(SAMPLE);
        assert_eq!(
            e.subject,
            "[PATCH] object-name: fix resolution of object names containing curly braces"
        );
    }

    #[test]
    fn parse_author() {
        let e = parse_email(SAMPLE);
        assert_eq!(
            e.author,
            "Elijah Newren via GitGitGadget <gitgitgadget@gmail.com>"
        );
    }

    #[test]
    fn parse_date() {
        let e = parse_email(SAMPLE);
        assert_eq!(e.date, "2025-01-01T02:53:09Z");
    }

    #[test]
    fn parse_message_id() {
        let e = parse_email(SAMPLE);
        assert_eq!(
            e.message_id,
            "pull.1844.git.1735699989371.gitgitgadget@gmail.com"
        );
    }

    #[test]
    fn parse_body_starts_after_thread_line() {
        let e = parse_email(SAMPLE);
        assert!(
            e.body.starts_with("From: Elijah Newren"),
            "unexpected body start: {:?}",
            &e.body[..e.body.len().min(80)]
        );
    }
}
