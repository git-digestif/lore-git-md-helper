use anyhow::Result;
use mail_parser::MimeHeaders;

pub mod cat_file;
pub mod datekey;
pub mod git_util;

pub mod fast_import;

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
        let from = escape_markdown_table_cell(&format_address(addr));
        md.push_str(&format!("| **From** | {from} |\n"));
    }

    if let Some(to) = message.to() {
        let to_addrs = escape_markdown_table_cell(
            &to.iter().map(format_address).collect::<Vec<_>>().join(", "),
        );
        md.push_str(&format!("| **To** | {to_addrs} |\n"));
    }

    if let Some(date) = message.date() {
        let date = escape_markdown_table_cell(&date.to_rfc3339());
        md.push_str(&format!("| **Date** | {date} |\n"));
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
            let link_text = escape_markdown_link_text(&filename);
            let link_target = encode_markdown_link_destination(&filename);
            md.push_str(&format!(
                "- [{}]({}) ({})\n",
                link_text, link_target, content_type
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

fn escape_markdown_table_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\r' | '\n' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_markdown_link_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            _ => out.push(ch),
        }
    }
    out
}

fn encode_markdown_link_destination(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").unwrap();
        }
    }
    out
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
    Code(Vec<String>),
    Quote(Vec<Block>),
}

fn format_body_content(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let blocks = parse_blocks(&lines);
    render_blocks(&blocks)
}

// --- Line-level predicates ---------------------------------------------------

fn is_snip_start(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "-- snip --" || trimmed == "-- snipsnap --"
}

fn is_diff_start(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("diff --cc")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@ ")
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
        || line.starts_with("new file mode")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("similarity index")
        || line.starts_with("dissimilarity index")
        || line.starts_with("rename from")
        || line.starts_with("rename to")
        || line.starts_with("copy from")
        || line.starts_with("copy to")
        || line.starts_with("Binary files")
        || line.starts_with("GIT binary patch")
}

fn is_quote_line(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// Could this line be part of a headerless diff?
/// Matches the standard diff line prefixes: ` `, `+`, or `-`.
fn is_possible_diff_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    !bytes.is_empty() && matches!(bytes[0], b' ' | b'+' | b'-')
}

/// Dead-giveaway pattern: a diff change prefix (`+` or `-`) followed by a tab.
fn has_diff_tab_signal(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'+' | b'-') && bytes[1] == b'\t'
}

fn is_indented(line: &str) -> bool {
    !line.is_empty() && (line.starts_with(' ') || line.starts_with('\t'))
}

fn is_code_start(line: &str) -> bool {
    match line.trim_start().chars().next() {
        Some(c) => !c.is_alphanumeric() && !"-.,:;!?\"'()[]{}".contains(c),
        None => false,
    }
}

fn is_code_like(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with("if ")
        || trimmed.starts_with("for ")
        || trimmed.starts_with("while ")
        || trimmed.starts_with("function ")
        || trimmed.starts_with("def ")
        || trimmed.starts_with("class ")
        || trimmed.contains("->")
        || trimmed.contains("=>")
        || trimmed.contains("::")
    {
        return true;
    }
    let special = trimmed
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    let total = trimmed.len();
    total > 0 && special * 100 / total > 30
}

fn strip_one_quote_level(line: &str) -> &str {
    let s = line.trim_start();
    match s.strip_prefix('>') {
        Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
        None => line,
    }
}

fn is_list(lines: &[&str]) -> bool {
    let mut iter = lines.iter().filter(|l| !l.trim().is_empty());
    let first = match iter.next() {
        Some(line) => line.trim_start(),
        None => return false,
    };
    let marker = if first.starts_with("- ") {
        '-'
    } else if first.starts_with("* ") {
        '*'
    } else {
        return false;
    };
    let mut expected_indent = None;
    for line in iter {
        if line.trim_start().starts_with(marker) {
            continue;
        }
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let expected = expected_indent.get_or_insert(indent);
        if indent != *expected {
            return false;
        }
    }
    true
}

fn should_fence_code_block(lines: &[String]) -> bool {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.as_str())
        .filter(|l| !l.trim().is_empty())
        .collect();
    if non_empty.len() < 2 {
        return false;
    }

    let first_chars: Vec<char> = non_empty
        .iter()
        .filter_map(|l| l.trim_start().chars().next())
        .collect();
    if first_chars.len() >= 2 {
        let first = first_chars[0];
        if !first.is_alphanumeric() {
            let matching = first_chars.iter().filter(|&&c| c == first).count();
            if matching * 100 / first_chars.len() > 50 {
                return true;
            }
        }
    }

    // Box-drawing characters
    if lines.iter().any(|l| {
        l.contains('│')
            || l.contains('─')
            || l.contains('┌')
            || l.contains('└')
            || l.contains('├')
            || l.contains('┤')
    }) {
        return true;
    }

    let indented = non_empty
        .iter()
        .filter(|l| l.starts_with("    ") || l.starts_with('\t'))
        .count();
    indented * 100 / non_empty.len() > 60
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
        if is_snip_start(line) {
            i += consume_snip(lines, i, &mut blocks);
            continue;
        }
        if is_diff_start(line) {
            let n = consume_diff(lines, i, &mut blocks);
            if n > 0 {
                i += n;
                continue;
            }
            // Not a real diff start (no continuation lines followed);
            // fall through to prose below.
        }
        if is_quote_line(line) {
            i += consume_quote(lines, i, &mut blocks);
            continue;
        }
        {
            let n = consume_headerless_diff(lines, i, &mut blocks);
            if n > 0 {
                i += n;
                continue;
            }
        }
        if is_indented(line) {
            i += consume_indented(lines, i, &mut blocks);
            continue;
        }
        if is_code_start(line) {
            i += try_consume_code(lines, i, &mut blocks);
            continue;
        }

        blocks.push(Block::Prose(vec![line.to_string()]));
        i += 1;
    }

    blocks
}

/// Try to consume a headerless diff block — a diff hunk quoted without its
/// `@@`/`---`/`+++` headers.  We recognise it by the "dead giveaway" of a
/// diff change prefix (`+` or `-`) immediately followed by a tab character.
///
/// All non-empty lines in the block must look like diff lines (start with
/// ` `, `+`, or `-`).  At least two non-empty lines must be present, with
/// at least one `+` or `-` change line among them.
/// Returns 0 when the block does not look like a headerless diff.
fn consume_headerless_diff(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut count = 0;
    let mut consecutive_empty = 0;
    let mut has_tab_signal = false;
    let mut has_change = false;
    let mut non_empty_count = 0usize;

    for &line in &lines[start..] {
        if line.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                break;
            }
            count += 1;
            continue;
        }
        consecutive_empty = 0;

        if !is_possible_diff_line(line) {
            break;
        }

        non_empty_count += 1;
        let bytes = line.as_bytes();
        if matches!(bytes[0], b'+' | b'-') {
            has_change = true;
        }
        if has_diff_tab_signal(line) {
            has_tab_signal = true;
        }

        count += 1;
    }

    if !has_tab_signal || !has_change || non_empty_count < 2 {
        return 0;
    }

    let collected: Vec<String> = lines[start..start + count]
        .iter()
        .map(|l| l.to_string())
        .collect();
    blocks.push(Block::Diff(collected));
    count
}

fn consume_snip(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let first = lines[start].trim();

    if first == "-- snipsnap --" {
        let code: Vec<String> = lines[start + 1..].iter().map(|l| l.to_string()).collect();
        blocks.push(Block::Code(code));
        return lines.len() - start;
    }

    // -- snip -- ... -- snap --
    let mut code = Vec::new();
    for (j, &line) in lines[start + 1..].iter().enumerate() {
        if line.trim() == "-- snap --" {
            blocks.push(Block::Code(code));
            return j + 2;
        }
        code.push(line.to_string());
    }
    blocks.push(Block::Code(code));
    lines.len() - start
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

    if count > 0 {
        blocks.push(Block::Diff(collected));
    }
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

fn consume_indented(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut collected: Vec<String> = Vec::new();
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
        if !is_indented(line) {
            break;
        }
        collected.push(line.to_string());
        count += 1;
    }

    let non_empty: Vec<&str> = collected
        .iter()
        .map(|s| s.as_str())
        .filter(|l| !l.trim().is_empty())
        .collect();

    if non_empty.len() >= 2 && !is_list(&non_empty) {
        blocks.push(Block::Code(collected));
    } else {
        for line in &collected {
            if line.is_empty() {
                blocks.push(Block::Blank);
            } else {
                blocks.push(Block::Prose(vec![line.clone()]));
            }
        }
    }
    count
}

fn try_consume_code(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut collected: Vec<String> = Vec::new();
    let mut count = 0;
    let mut consecutive_empty = 0;

    for &line in &lines[start..] {
        if line.trim().is_empty() {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                break;
            }
            collected.push(line.to_string());
            count += 1;
        } else {
            consecutive_empty = 0;
            collected.push(line.to_string());
            count += 1;
            if !is_code_like(line) && count > 3 {
                break;
            }
        }
    }

    if should_fence_code_block(&collected) {
        blocks.push(Block::Code(collected));
        count
    } else {
        // Only emit first line as prose; let caller re-evaluate the rest
        blocks.push(Block::Prose(vec![lines[start].to_string()]));
        1
    }
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
            Block::Code(lines) => {
                out.push_str("```\n");
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

    fn create_test_email_with_headers(from: &str, to: &str, subject: &str, body: &str) -> Vec<u8> {
        format!(
            "From: {from}\r\n\
             To: {to}\r\n\
             Subject: {subject}\r\n\
             Date: Mon, 9 Dec 2024 10:00:00 +0000\r\n\
             \r\n\
             {body}"
        )
        .into_bytes()
    }

    fn create_test_email(subject: &str, body: &str) -> Vec<u8> {
        create_test_email_with_headers("test@example.com", "user@example.com", subject, body)
    }

    fn create_test_email_with_attachment(
        subject: &str,
        body: &str,
        filename: &str,
        content_type: &str,
    ) -> Vec<u8> {
        format!(
            "From: test@example.com\r\n\
             To: user@example.com\r\n\
             Subject: {subject}\r\n\
             Date: Mon, 9 Dec 2024 10:00:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
             \r\n\
             --BOUNDARY\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             {body}\r\n\
             --BOUNDARY\r\n\
             Content-Type: {content_type}; name=\"{filename}\"\r\n\
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             Content-Transfer-Encoding: 7bit\r\n\
             \r\n\
             attachment body\r\n\
             --BOUNDARY--\r\n"
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
    fn test_header_table_escapes_raw_pipes() {
        let email = create_test_email_with_headers(
            r#""Terrence | Wolf1098" <wolf@example.com>"#,
            r#""Review | List" <list@example.com>"#,
            "Test Subject",
            "Simple message.",
        );
        let result = parse_and_convert(&email);

        assert!(result.contains(r"| **From** | Terrence \| Wolf1098 <wolf@example.com> |"));
        assert!(result.contains(r"| **To** | Review \| List <list@example.com> |"));
    }

    #[test]
    fn test_attachment_links_escape_label_and_target() {
        let email = create_test_email_with_attachment(
            "Attachment test",
            "See attachment.",
            "weird [name] (v1) | final.patch",
            "text/x-patch",
        );
        let result = parse_and_convert(&email);

        assert!(result.contains("**Attachments:**"));
        assert!(result.contains(
            r"- [weird \[name\] (v1) | final.patch](weird%20%5Bname%5D%20%28v1%29%20%7C%20final.patch) (text)"
        ));
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

    #[test]
    fn test_snip_snap_markers() {
        let body = concat!(
            "Check this output:\n",
            "\n",
            "-- snip --\n",
            "some code\n",
            "  indented\n",
            "    more\n",
            "-- snap --\n",
            "\n",
            "And that's it.\n",
        );
        let email = create_test_email("Code snippet", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("```\n"), "Should have code fence");
        assert!(
            result.contains("some code"),
            "Should contain snipped content"
        );
        assert!(result.contains("  indented"), "Should preserve indentation");
        assert!(
            result.contains("And that's it."),
            "Should have text after snap"
        );
    }

    #[test]
    fn test_snipsnap_marker() {
        let body = concat!(
            "Everything below is code:\n",
            "\n",
            "-- snipsnap --\n",
            "line 1\n",
            "line 2\n",
            "line 3\n",
        );
        let email = create_test_email("Rest is code", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("```\n"), "Should have code fence");
        assert!(result.contains("line 1"), "Should contain all lines");
        assert!(result.contains("line 3"), "Should fence to end");
    }

    #[test]
    fn test_quoted_snipsnap() {
        let body = concat!(
            "Regular text.\n",
            "\n",
            "> Someone said:\n",
            "> -- snipsnap --\n",
            "> quoted code\n",
            "> more code\n",
            "\n",
            "My reply continues.\n",
        );
        let email = create_test_email("Quoted code", body);
        let result = parse_and_convert(&email);

        assert!(
            result.contains("> ```\n"),
            "Should have fenced code in quote"
        );
        assert!(
            result.contains("> quoted code"),
            "Should contain quoted code"
        );
        assert!(
            result.contains("My reply continues."),
            "Should have text after quote"
        );
    }

    #[test]
    fn test_indented_block() {
        let body = concat!(
            "Command output:\n",
            "\n",
            " $ git status\n",
            " On branch main\n",
            " nothing to commit\n",
            "\n",
            "Done.\n",
        );
        let email = create_test_email("Command output", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("```\n"), "Should have code fence");
        assert!(result.contains(" $ git status"), "Should contain command");
        assert!(result.contains(" On branch main"), "Should contain output");
        assert!(result.contains("Done."), "Should have text after block");
    }

    #[test]
    fn test_ascii_art_in_quotes() {
        let body = concat!(
            "Here's the diagram:\n",
            "\n",
            ">    A---B---C\n",
            ">         \\\n",
            ">          D---E\n",
            "\n",
            "Makes sense?\n",
        );
        let email = create_test_email("ASCII diagram", body);
        let result = parse_and_convert(&email);

        assert!(
            result.contains("> ```\n"),
            "Should have fenced code in quote"
        );
        assert!(
            result.contains(">    A---B---C"),
            "Should contain ASCII art line"
        );
        assert!(result.contains("> ```"), "Should close fence");
    }

    #[test]
    fn test_indented_list_not_fenced() {
        let body = concat!(
            "Changes:\n",
            "\n",
            " - first item\n",
            " - second item\n",
            " - third item\n",
            "\n",
            "End.\n",
        );
        let email = create_test_email("List test", body);
        let result = parse_and_convert(&email);

        assert!(!result.contains("```"), "Lists should NOT be fenced");
        assert!(result.contains("- first item"), "Should contain list items");
    }

    #[test]
    fn test_mixed_content() {
        let body = concat!(
            "Some text.\n",
            "\n",
            " $ command\n",
            " output\n",
            "\n",
            "> Quote\n",
            "> more quote\n",
            "\n",
            "Regular paragraph.\n",
        );
        let email = create_test_email("Mixed content", body);
        let result = parse_and_convert(&email);

        assert!(result.contains("Some text."), "Should have regular text");
        assert!(result.contains("```\n"), "Should have fenced code");
        assert!(result.contains("> Quote"), "Should have quotes");
        assert!(
            result.contains("Regular paragraph."),
            "Should have final text"
        );
    }

    #[test]
    fn test_diff_with_extended_headers() {
        let body = concat!(
            "New file patch:\n",
            "\n",
            "diff --git a/newfile.sh b/newfile.sh\n",
            "new file mode 100755\n",
            "index 0000000..abc1234\n",
            "--- /dev/null\n",
            "+++ b/newfile.sh\n",
            "@@ -0,0 +1,2 @@\n",
            "+#!/bin/sh\n",
            "+echo hello\n",
        );
        let email = create_test_email("New file patch", body);
        let result = parse_and_convert(&email);

        let diff_count = result.matches("```diff").count();
        assert_eq!(diff_count, 1, "Should have exactly one diff block");
        assert!(
            result.contains("new file mode 100755"),
            "Should contain new file mode"
        );
        assert!(result.contains("+echo hello"), "Should contain added line");
    }

    #[test]
    fn test_diff_with_rename() {
        let body = concat!(
            "diff --git a/old.c b/new.c\n",
            "similarity index 95%\n",
            "rename from old.c\n",
            "rename to new.c\n",
            "index abc1234..def5678\n",
            "--- a/old.c\n",
            "+++ b/new.c\n",
            "@@ -1,3 +1,4 @@\n",
            " int main() {\n",
            "+    setup();\n",
            "     return 0;\n",
            " }\n",
        );
        let email = create_test_email("Rename patch", body);
        let result = parse_and_convert(&email);

        let diff_count = result.matches("```diff").count();
        assert_eq!(diff_count, 1, "Should have exactly one diff block");
        assert!(
            result.contains("rename from old.c"),
            "Should contain rename from"
        );
        assert!(
            result.contains("rename to new.c"),
            "Should contain rename to"
        );
    }

    #[test]
    fn test_headerless_diff_with_tabs() {
        // Simulates a quoted diff hunk without @@ headers — the tab after
        // the diff-line prefix (+/-/ ) is the "dead giveaway".
        // Real emails use ">  \t" (two spaces: quote separator + diff context).
        let body = "Reviewer wrote:\n\
                     \n\
                     >  \tgrep \"usage\" expect\n\
                     > +\ttest_must_fail git merge 2>err &&\n\
                     > +\ttest_cmp expect err\n\
                     >  \t}\n\
                     \n\
                     Looks good.\n";
        let email = create_test_email("Review", body);
        let result = parse_and_convert(&email);

        let diff_count = result.matches("```diff").count();
        assert_eq!(
            diff_count, 1,
            "Tab-signaled lines should form one diff block"
        );
        // Context lines must be inside the fence, not orphaned outside it
        assert!(
            result.contains("> ```diff\n>  \tgrep"),
            "Context line before changes should be inside diff fence"
        );
        assert!(
            result.contains(">  \t}\n> ```"),
            "Context line after changes should be inside diff fence"
        );
        assert!(
            result.contains("+\ttest_must_fail"),
            "Should contain added line"
        );
        assert!(
            result.contains("Looks good."),
            "Should have prose after quote"
        );
    }

    #[test]
    fn test_false_diff_start_no_hang() {
        // "@@ something" looks like a diff hunk header to is_diff_start
        // but has no continuation lines.  This must not cause an infinite loop.
        let body = " \treturn 0;\n\
                     }\n\
                     \n\
                     ---\n\
                     @@ something\n";
        let email = create_test_email("patch trailer", body);
        let result = parse_and_convert(&email);
        assert!(
            result.contains("@@ something"),
            "marker should appear in output"
        );
    }
}
