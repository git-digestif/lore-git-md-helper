# agents.md — lore-git-md-helper

## Project Purpose

The Git mailing list (git@vger.kernel.org) is a firehose of highly technical
discussion: patches, reviews, design debates, bug reports. Its volume makes it
essentially a full-time job to keep up with. This project exists to make that
mailing list AI-accessible so that language models can summarize threads,
surface key decisions, and help maintainers and contributors stay informed
without reading every message.

The mailing list archive is available as a Git repository where every commit
replaces a single file `m` with the Mbox-formatted content of the next email.
The diffs between commits are meaningless (they show a full-file rewrite each
time). The emails are stored chronologically with no structural threading;
thread relationships must be reconstructed from `Message-Id`, `In-Reply-To`,
and `References` headers.

This repository is tackling the problem in stages:

1. **Mbox → Markdown conversion** (current focus): Parse individual emails
   and produce clean Markdown that preserves the semantic structure — nested
   quoting, inline diffs, ASCII art, code blocks, snip/snap markers — while
   being far more digestible for AI than raw Mbox.

2. **Date-based file hierarchy** (not yet implemented): Reorganize the output
   into a sparse-checkoutable tree like `2026/01/12/04-46-13.md` (derived
   from the email's parsed `Date:` header in UTC, with conflict resolution
   for duplicate timestamps). Each file gets YAML front matter containing at
   minimum the `Message-Id` and, for replies, a reference to the thread root.
   This layout lets GitHub index the entire corpus at the tip revision,
   enabling vector search over the full archive.

3. **Message-Id mapping** (not yet implemented): Generate a mapping — likely
   Git notes — that annotates each commit/blob with its `Message-Id` string,
   making it possible to go from a `Message-Id` or `In-Reply-To` reference
   directly to the email content.

4. **Thread reconstruction** (not yet implemented): Use the `Message-Id` /
   `In-Reply-To` / `References` headers to reconstruct conversation threads
   and represent them in Markdown (exact representation TBD).

5. **AI-generated summaries** (future): Per-email summaries, per-thread
   summaries (updated as new replies arrive), and roll-up summaries at daily,
   weekly, monthly, and yearly granularity. This will require significant
   iteration and fine-tuning.

## Current State

The project is in early development. Only stage 1 is partially implemented:
a Rust CLI (`mbox2md`) that reads a single `.eml` or `.mbox` file and writes
a Markdown file. The converter handles:

- Email headers rendered as a Markdown table (From, To, Date, Message-ID)
- Nested email quoting (`>`, `> >`, `> > >`, …) preserved as Markdown
  blockquotes
- Inline diffs detected and wrapped in ` ```diff ` fenced code blocks
- `-- snip --` / `-- snap --` / `-- snipsnap --` markers converted to
  fenced code blocks (including inside quoted regions)
- ASCII art and indented command output fenced as code blocks
- Indented list items recognized and left unfenced
- HTML email bodies converted to plain text via `html2text`

## Technology

Rust (edition 2024), using:
- `mail-parser` for RFC 5322 / Mbox parsing
- `html2text` for HTML → plain text fallback
- `clap` for CLI argument parsing
- `anyhow` for error handling

Rust was chosen for robustness, type safety, and raw text-parsing speed.
This is a pragmatic choice, not a firm commitment — if a different language
proves better suited for later stages (e.g., tighter integration with AI
tooling), that is open for discussion.

## Conventions

- The converter must never corrupt diffs. Diff content is fenced verbatim;
  no reformatting, no line rewrapping.
- ASCII art and preformatted blocks must be detected and preserved inside
  fenced code blocks.
- Quoted text retains its nested `>` structure. Code blocks inside quotes
  get fenced within the blockquote.
- The output should be maximally useful to language models: clear structure,
  meaningful front matter, no ambiguous formatting.

## Commit Hygiene

The maintainer has a strong preference for a clean, reviewable commit
history.  Every commit must be a single logical change — never lump a
bug fix, a refactor, and a new feature into one commit.

- **One purpose per commit.**  A bug fix is one commit; adding a test for
  it may be the same commit or a separate one, but an unrelated feature
  must not be squashed in.
- **Prefer smaller commits.**  When changes are well-separated they are
  naturally small, which makes review easier and keeps bisectability.
- **Commit messages** should have an informative first line, a blank line,
  then a body wrapped at 72 columns explaining *why* (not just *what*).
- **Trailers**: `Assisted-by: Claude Opus 4.6` (or whichever model) and
  `Signed-off-by:` with the identity from `git var GIT_COMMITTER_IDENT`.
- Do **not** include a `Co-authored-by: Copilot <…>` trailer — the
  `Assisted-by` trailer already conveys the information.

## Building and Testing

```sh
cargo build
cargo test
```

There are unit tests in `src/lib.rs` covering diff detection, nested
quoting, snip/snap markers, indented blocks, ASCII art, and mixed content.
Run them before and after any change.

## Key Files

- `src/lib.rs` — converter library (block parser + renderer + tests)
- `src/bin/mbox2md.rs` — CLI entry point for mbox-to-markdown conversion
- `src/bin/msgid-notes.rs` — tool to map Message-IDs → blob OIDs via Git notes
- `Cargo.toml` — dependencies and project metadata
- `sample4.md` — example converter output (a real Git mailing list patch)
