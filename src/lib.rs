use anyhow::Result;
use mail_parser::MimeHeaders;

pub fn email_to_markdown(message: &mail_parser::Message) -> Result<String> {
    let mut md = String::new();

    // Subject as title
    let subject = message.subject().unwrap_or("(No Subject)");
    md.push_str(&format!("# {subject}\n\n"));

    // Headers as table
    md.push_str("| Header | Value |\n");
    md.push_str("|--------|-------|\n");

    if let Some(from) = message.from()
        && let Some(addr) = from.first()
    {
        md.push_str(&format!("| **From** | {} |\n", format_address(addr)));
    }

    if let Some(to) = message.to() {
        let to_addrs: Vec<String> = to.iter().map(format_address).collect();
        md.push_str(&format!("| **To** | {} |\n", to_addrs.join(", ")));
    }

    if let Some(date) = message.date() {
        md.push_str(&format!("| **Date** | {} |\n", date.to_rfc3339()));
    }

    if let Some(message_id) = message.message_id() {
        md.push_str(&format!("| **Message-ID** | `{message_id}` |\n"));
    }

    md.push_str("\n---\n\n");

    // Body content
    let body = extract_body(message)?;
    md.push_str(&body);
    md.push_str("\n\n");

    // Attachments
    let attachments = collect_attachments(message);
    if !attachments.is_empty() {
        md.push_str("---\n\n");
        md.push_str("**Attachments:**\n\n");
        for (filename, content_type) in attachments {
            md.push_str(&format!(
                "- [{}]({}) ({})\n",
                filename, filename, content_type
            ));
        }
    }

    Ok(md)
}

fn format_address(addr: &mail_parser::Addr) -> String {
    if let Some(name) = addr.name() {
        format!("{} <{}>", name, addr.address().unwrap_or(""))
    } else {
        addr.address().unwrap_or("").to_string()
    }
}

fn extract_body(message: &mail_parser::Message) -> Result<String> {
    if let Some(text_body) = message.body_text(0) {
        return Ok(format_body_content(&text_body));
    }

    if let Some(html_body) = message.body_html(0) {
        let text = html2text::from_read(html_body.as_bytes(), 80)?;
        return Ok(format_body_content(&text));
    }

    Ok("(No body content)".to_string())
}

// ============================================================================
// Block-based body parser (classify → group → render)
// ============================================================================

#[derive(Debug)]
enum Block {
    Blank,
    Prose(Vec<String>),
    Diff(Vec<String>),
    Quote(Vec<Block>),
}

fn format_body_content(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let blocks = parse_blocks(&lines);
    render_blocks(&blocks)
}

// --- Line-level predicates ---------------------------------------------------

fn is_diff_start(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("diff --cc")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@")
}

fn is_diff_continuation(line: &str) -> bool {
    line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@ ")
        || line.starts_with('+')
        || line.starts_with('-')
        || line.starts_with(' ')
}

fn is_quote_line(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn strip_one_quote_level(line: &str) -> &str {
    let s = line.trim_start();
    match s.strip_prefix('>') {
        Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
        None => line,
    }
}

// --- Block grouping ----------------------------------------------------------

fn parse_blocks(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if line.trim().is_empty() {
            blocks.push(Block::Blank);
            i += 1;
            continue;
        }
        if is_diff_start(line) {
            i += consume_diff(lines, i, &mut blocks);
            continue;
        }
        if is_quote_line(line) {
            i += consume_quote(lines, i, &mut blocks);
            continue;
        }

        blocks.push(Block::Prose(vec![line.to_string()]));
        i += 1;
    }

    blocks
}

fn consume_diff(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut collected = Vec::new();
    let mut count = 0;
    let mut consecutive_empty = 0;

    for &line in &lines[start..] {
        if line.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                break;
            }
            collected.push(line.to_string());
            count += 1;
            continue;
        }
        consecutive_empty = 0;
        if !is_diff_continuation(line) {
            break;
        }
        collected.push(line.to_string());
        count += 1;
    }

    blocks.push(Block::Diff(collected));
    count
}

fn consume_quote(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut raw: Vec<&str> = Vec::new();
    let mut count = 0;

    for (j, &line) in lines[start..].iter().enumerate() {
        if is_quote_line(line) {
            raw.push(line);
            count = j + 1;
        } else if line.trim().is_empty() {
            // Absorb blank only if another quoted line follows
            let rest = &lines[start + j + 1..];
            let has_more_quotes = rest
                .iter()
                .take_while(|l| l.trim().is_empty() || is_quote_line(l))
                .any(|l| is_quote_line(l));
            if has_more_quotes {
                raw.push(line);
                count = j + 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // Strip one > level and recursively parse
    let stripped: Vec<String> = raw
        .iter()
        .map(|l| strip_one_quote_level(l).to_string())
        .collect();
    let refs: Vec<&str> = stripped.iter().map(|s| s.as_str()).collect();
    blocks.push(Block::Quote(parse_blocks(&refs)));
    count
}

// --- Rendering ---------------------------------------------------------------

fn render_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            Block::Blank => out.push('\n'),
            Block::Prose(lines) => {
                for line in lines {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::Diff(lines) => {
                out.push_str("```diff\n");
                for line in lines {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            Block::Quote(inner) => {
                let rendered = render_blocks(inner);
                for line in rendered.lines() {
                    if line.is_empty() {
                        out.push_str(">\n");
                    } else {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                out.push('\n');
            }
        }
    }
    out
}

fn collect_attachments(message: &mail_parser::Message) -> Vec<(String, String)> {
    let mut attachments = Vec::new();

    for attachment in message.attachments() {
        let filename = attachment
            .attachment_name()
            .unwrap_or("unnamed")
            .to_string();

        let content_type = attachment
            .content_type()
            .map(|ct| ct.ctype().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        attachments.push((filename, content_type));
    }

    attachments
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    fn create_test_email(subject: &str, body: &str) -> Vec<u8> {
        format!(
            "From: test@example.com\r\n\
             To: user@example.com\r\n\
             Subject: {}\r\n\
             Date: Mon, 9 Dec 2024 10:00:00 +0000\r\n\
             \r\n\
             {}",
            subject, body
        )
        .into_bytes()
    }

    fn parse_and_convert(email_bytes: &[u8]) -> String {
        let message = MessageParser::default()
            .parse(email_bytes)
            .expect("Failed to parse test email");
        email_to_markdown(&message).expect("Failed to convert to markdown")
    }

    #[test]
    fn test_headers_as_table() {
        let body = "Simple message.";
        let email = create_test_email("Test Subject", body);
        let result = parse_and_convert(&email);

        assert!(
            result.contains("| Header | Value |"),
            "Should have table header"
        );
        assert!(
            result.contains("| **From** | test@example.com |"),
            "Should have From in table"
        );
        assert!(
            result.contains("| **To** | user@example.com |"),
            "Should have To in table"
        );
        assert!(result.contains("| **Date** |"), "Should have Date in table");
    }

    #[test]
    fn test_diff_detection() {
        let body = concat!(
            "Here's a patch:\n",
            "\n",
            "diff --git a/file.c b/file.c\n",
            "index abc123..def456 100644\n",
            "--- a/file.c\n",
            "+++ b/file.c\n",
            "@@ -1,3 +1,4 @@\n",
            " int main() {\n",
            "+    printf(\"hello\");\n",
            "     return 0;\n",
            " }\n",
        );
        let email = create_test_email("Patch submission", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("```diff\n"), "Should have diff fence");
        assert!(result.contains("diff --git"), "Should contain diff header");
        assert!(result.contains("+    printf("), "Should contain added line");
    }

    #[test]
    fn test_diff_with_empty_lines() {
        let body = concat!(
            "Patch with empty lines:\n",
            "\n",
            "diff --git a/test.sh b/test.sh\n",
            "--- a/test.sh\n",
            "+++ b/test.sh\n",
            "@@ -1,5 +1,6 @@\n",
            " line1\n",
            "\n",
            "+newline\n",
            " line2\n",
        );
        let email = create_test_email("Patch with blanks", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("```diff\n"), "Should have diff fence");
        let diff_count = result.matches("```diff").count();
        assert_eq!(diff_count, 1, "Should have exactly one diff block");
    }

    #[test]
    fn test_nested_quotes() {
        let body = concat!(
            "I disagree.\n",
            "\n",
            "> Alice wrote:\n",
            "> > Bob said:\n",
            "> > > Original message\n",
            "> >\n",
            "> > Bob's reply\n",
            ">\n",
            "> Alice's reply\n",
            "\n",
            "My response.\n",
        );
        let email = create_test_email("Re: Discussion", body);
        let result = parse_and_convert(&email);

        assert!(
            result.contains("> > > Original message"),
            "Should have triple-nested quote"
        );
        assert!(
            result.contains("> > Bob's reply"),
            "Should have double-nested quote"
        );
        assert!(
            result.contains("> Alice's reply"),
            "Should have single-level quote"
        );
        assert!(result.contains("My response."), "Should have unquoted text");
    }
}
