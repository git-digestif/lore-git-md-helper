//! In-memory attribution annotation for the summarization pipeline.
//!
//! When a reply email is fed to the summarizer, the sender's mail
//! client may or may not have prepended an "On <date>, <person>
//! wrote:" line above the quoted text.  Downstream summarization
//! agents need to know who wrote the quoted text; we can't rely on
//! the sender to include that information, so we insert it ourselves
//! from the thread graph before handing the markdown to the LLM.
//!
//! **This annotation is applied in memory only, in `summarize.rs`
//! and `digestive.rs`, when building LLM prompts.  It is never
//! written back to the corpus.**  The email markdown committed to
//! the corpus (produced by `mbox2md` + `batch_import`) stays
//! unchanged.
//!
//! Only first-level quotes are annotated: deeper nesting quotes
//! grandparent (and further ancestor) text whose author we may not
//! have in scope.  The insertion is idempotent: if the immediately
//! preceding non-blank line already matches a `wrote:` attribution
//! pattern, no new line is added, so mail-client-generated
//! attributions and our own do not stack.

use std::sync::OnceLock;

use regex::Regex;

/// Strip a trailing `<addr@example.com>` from a `From:` header value
/// and return the display name, trimmed.  Falls back to the raw
/// input if no angle-bracket address is present.
///
/// Examples:
/// - `"Junio C Hamano <gitster@pobox.com>"` -> `"Junio C Hamano"`
/// - `"gitster@pobox.com"` (bare address) -> `"gitster@pobox.com"`
pub fn display_name(from: &str) -> &str {
    let name = match from.rfind('<') {
        Some(pos) => from[..pos].trim(),
        None => from.trim(),
    };
    if name.is_empty() { from.trim() } else { name }
}

/// Insert unconditional attribution lines into `md`:
///
/// - `[<parent_from display> wrote:]` above every first-level quote
///   block that is not already preceded by an attribution line.
/// - `[<self_from display> wrote:]` above every stretch of unquoted
///   text that immediately follows a quote block, so a reply
///   author's own observations are never conflated with the text
///   they were quoting.
///
/// A first-level quote block is a maximal run of consecutive lines
/// starting with `>`, where at least one line is depth 1
/// (i.e., `> ` or a bare `>`, not `> >`).  Each insertion is
/// idempotent: if the preceding non-blank line already ends with
/// `wrote:` (case-insensitive), or if the very line we would
/// annotate is itself an attribution line, the new line is not
/// added.
///
/// `parent_from`, when `None`, disables quote-block annotation
/// (used for thread roots that quote nothing).  `self_from`, when
/// `None`, disables the transition-back-to-unquoted annotation
/// (used when we cannot identify the current email's author).
///
/// Only the display name portion of each `From:` value is used
/// (any trailing `<addr>` is stripped).
///
/// Returns `md` unchanged when it contains no annotations to
/// insert, so callers can call this on every email without
/// worrying about extra allocations for the common non-reply case.
pub fn annotate_attribution(
    md: &str,
    self_from: Option<&str>,
    parent_from: Option<&str>,
) -> String {
    let self_line = self_from.map(|s| format!("[{} wrote:]", display_name(s)));
    let parent_line = parent_from.map(|p| format!("[{} wrote:]", display_name(p)));

    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut prev_was_quote_block = false;
    while i < lines.len() {
        if is_quote_line(lines[i]) {
            let (block_end, has_first_level) = scan_quote_block(&lines, i);
            if let Some(ref pl) = parent_line
                && has_first_level
                && !preceded_by_attribution(&out)
            {
                out.push(pl.clone());
            }
            for line in &lines[i..block_end] {
                out.push((*line).to_string());
            }
            i = block_end;
            prev_was_quote_block = true;
        } else if prev_was_quote_block && !lines[i].trim().is_empty() {
            if let Some(ref sl) = self_line
                && !preceded_by_attribution(&out)
                && !is_attribution_line(lines[i])
            {
                out.push(sl.clone());
            }
            out.push(lines[i].to_string());
            i += 1;
            prev_was_quote_block = false;
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }

    let mut joined = out.join("\n");
    if md.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// True for any line that looks like part of a quote block: starts
/// with `>` (with or without a following space).
fn is_quote_line(line: &str) -> bool {
    line.starts_with('>')
}

/// True for a first-level (depth 1) quote line: `> foo`, or bare `>`.
/// Depth 2 lines like `> > foo` are not first-level.
fn is_first_level_quote(line: &str) -> bool {
    let stripped = match line.strip_prefix('>') {
        Some(s) => s,
        None => return false,
    };
    if stripped.is_empty() {
        return true; // bare `>`
    }
    if !stripped.starts_with(' ') && !stripped.starts_with('\t') {
        return true; // `>foo` without a space (some clients)
    }
    let after_space = stripped.trim_start_matches([' ', '\t']);
    !after_space.starts_with('>')
}

/// Scan from `start` while `lines[j]` are quote or blank-inside-block,
/// returning the exclusive end index and whether the block contained
/// any first-level line.
///
/// A blank line ends the block; a non-quote non-blank line also ends
/// it.  Empty quoted lines (`>` alone) are treated as quote lines.
fn scan_quote_block(lines: &[&str], start: usize) -> (usize, bool) {
    let mut j = start;
    let mut has_first_level = false;
    while j < lines.len() && is_quote_line(lines[j]) {
        if is_first_level_quote(lines[j]) {
            has_first_level = true;
        }
        j += 1;
    }
    (j, has_first_level)
}

/// True if the last non-blank line already ending `out` matches a
/// `wrote:` attribution pattern.
fn preceded_by_attribution(out: &[String]) -> bool {
    for line in out.iter().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        return is_attribution_line(t);
    }
    false
}

/// True if `line` (already trimmed or not) looks like a `wrote:`
/// attribution line, e.g. `"On Mon, Foo Bar wrote:"`, `"Weijie Yuan
/// wrote:"`, `"[Weijie Yuan wrote:]"`.  Matches any line that ends
/// with `wrote:` after optional trailing `]` and whitespace.
fn is_attribution_line(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)wrote:\s*\]?\s*$").unwrap());
    re.is_match(line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_strips_address() {
        assert_eq!(
            display_name("Junio C Hamano <gitster@pobox.com>"),
            "Junio C Hamano",
        );
        assert_eq!(display_name("  Foo Bar <x@y> "), "Foo Bar");
        assert_eq!(display_name("bare@address.com"), "bare@address.com");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn annotates_first_level_quote_when_no_existing_attribution() {
        let md = "Hi,\n\n> The latest release is out.\n> Please try it.\n\nThanks.\n";
        let out = annotate_attribution(md, None, Some("Junio C Hamano <j@x>"));
        assert!(
            out.contains("[Junio C Hamano wrote:]\n> The latest release is out."),
            "expected annotation inserted, got:\n{out}",
        );
    }

    #[test]
    fn idempotent_when_wrote_line_already_present() {
        let md = "On Mon, Junio C Hamano wrote:\n> Text.\n> More text.\n\nReply.\n";
        let out = annotate_attribution(md, None, Some("Junio C Hamano <j@x>"));
        assert_eq!(
            out, md,
            "an existing wrote: line must suppress our annotation",
        );
    }

    #[test]
    fn skips_pure_second_level_block() {
        // A block that is entirely `> > ...` should not be annotated:
        // it quotes the grandparent, whose name we do not have.
        let md = "First-level omitted, second-level:\n\n> > grandparent text\n> > more grandparent text\n\nEnd.\n";
        let out = annotate_attribution(md, None, Some("Someone <x@y>"));
        assert_eq!(out, md, "second-level-only block must be left alone");
    }

    #[test]
    fn annotates_mixed_depth_block_once() {
        let md = "> parent text\n> > grandparent text\n> more parent text\n\nEnd.\n";
        let out = annotate_attribution(md, None, Some("Parent <p@x>"));
        // The block should be annotated once at the top.
        assert!(out.starts_with("[Parent wrote:]\n> parent text\n"));
        // The nested `> >` line is left as-is; we do not annotate it.
        assert!(out.contains("> > grandparent text"));
    }

    #[test]
    fn annotates_multiple_interleaved_blocks_independently() {
        let md = "\
Response line one.

> quoted A
> more A

Response line two.

> quoted B
> more B

End.
";
        let out = annotate_attribution(md, None, Some("Alice <a@x>"));
        // Both blocks get their own attribution because each stands alone.
        assert_eq!(out.matches("[Alice wrote:]").count(), 2);
    }

    #[test]
    fn preserves_bare_quote_lines() {
        let md = ">\n> line after empty quote\n";
        let out = annotate_attribution(md, None, Some("Bob <b@x>"));
        assert!(out.starts_with("[Bob wrote:]\n>\n> line after empty quote"));
    }

    #[test]
    fn no_change_when_no_quote_at_all() {
        let md = "Plain body text with no quote lines.\n";
        assert_eq!(annotate_attribution(md, Some("Alice <a@x>"), None), md);
    }

    #[test]
    fn preserves_trailing_newline_semantics() {
        let md_with_nl = "> quoted\n";
        let md_without_nl = "> quoted";
        assert!(annotate_attribution(md_with_nl, None, Some("X <x@x>")).ends_with('\n'));
        assert!(!annotate_attribution(md_without_nl, None, Some("X <x@x>")).ends_with('\n'));
    }

    #[test]
    fn annotates_unquoted_stretch_after_quote_block() {
        let md = "> quoted from parent\n> more parent text\n\nReply author responds here.\n";
        let out = annotate_attribution(md, Some("Weijie Yuan <wy@x>"), Some("Junio <j@x>"));
        assert!(out.contains("[Junio wrote:]\n> quoted from parent"));
        assert!(
            out.contains("> more parent text\n\n[Weijie Yuan wrote:]\nReply author responds here."),
            "expected self-attribution after quote block, got:\n{out}",
        );
    }

    #[test]
    fn interleaved_quotes_get_alternating_attributions() {
        let md = "\
> first quote from parent

Reply chunk one.

> second quote from parent

Reply chunk two.
";
        let out = annotate_attribution(md, Some("Alice <a@x>"), Some("Bob <b@x>"));
        assert_eq!(out.matches("[Bob wrote:]").count(), 2);
        assert_eq!(out.matches("[Alice wrote:]").count(), 2);
    }

    #[test]
    fn no_self_attribution_when_body_has_no_quotes() {
        // Thread roots and pure prose replies should not get a
        // spurious `[Alice wrote:]` line; the header table already
        // identifies the sender, and there is no quote-vs-reply
        // ambiguity to disambiguate.
        let md = "Plain body text with no quote lines.\n";
        let out = annotate_attribution(md, Some("Alice <a@x>"), None);
        assert_eq!(out, md);
    }

    #[test]
    fn idempotent_self_attribution() {
        let md = "> parent line\n\nWeijie Yuan wrote:\nfollow-up text here\n";
        let out = annotate_attribution(md, Some("Weijie Yuan <wy@x>"), Some("Junio <j@x>"));
        assert!(
            !out.contains("[Weijie Yuan wrote:]"),
            "self-attribution must be suppressed when a wrote: line already precedes the unquoted stretch, got:\n{out}",
        );
    }

    #[test]
    fn parent_none_disables_quote_annotation_but_still_self_annotates_transitions() {
        // Odd case: a thread root that somehow contains a quote.
        // With parent_from = None, we skip the quote-block annotation
        // (since we do not know whose quote it is) but still
        // self-attribute the unquoted stretch that follows so the
        // reply author's own text is not lost.
        let md = "> stray quote\n\nMy own words.\n";
        let out = annotate_attribution(md, Some("Alice <a@x>"), None);
        assert!(!out.contains("[wrote:]"));
        assert!(!out.contains("[ wrote:]"));
        assert!(out.contains("[Alice wrote:]\nMy own words."));
    }
}
