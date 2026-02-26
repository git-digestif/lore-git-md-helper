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

Stage 1 (Mbox → Markdown) is implemented: `mbox2md` reads `.eml`/`.mbox`
files and writes Markdown preserving semantic structure.

A **RAG subsystem** provides local retrieval-augmented Q&A over the
converted Markdown corpus. It includes:

- `rag_db`: SQLite schema with FTS5 virtual table for full-text search
- `rag_parse`: regex-based extraction of subject, author, date,
  message-id, and body from the Markdown format
- `rag_ingest`: file-based and git-backed ingestion into the database,
  with incremental ingest via stored commit state; uses `diff-tree`
  for fast incremental change detection and defers FTS5 optimization
  until enough rows have accumulated
- `rag_query`: FTS5/BM25 retrieval with prompt assembly and Message-ID
  citations
- `rag_git`: `ls_tree` for full tree enumeration and `diff_tree` for
  incremental change detection between commits
- `lore-rag` binary: CLI with `ingest` and `query` subcommands

An **AI backend** module (`ai_backend`) provides a shared abstraction
over multiple LLM backends: OpenAI-compatible API, GitHub Copilot CLI,
Ollama, GitHub Models, and Azure OpenAI. It includes retry logic with
exponential backoff for 429 rate limits, 400 Bad Request, and 5xx
server errors, retry-after header support with a cap, and empty
response detection. API errors are propagated as hard failures rather
than producing error markers in the output.

A `msgid-notes` tool maps Message-IDs to blob OIDs via Git notes
(stage 3 groundwork).

## Technology

Rust (edition 2024), using:
- `mail-parser` for RFC 5322 / Mbox parsing
- `html2text` for HTML → plain text fallback
- `clap` for CLI argument parsing
- `anyhow` for error handling
- `rusqlite` (bundled, with FTS5) for the RAG database
- `regex` for Markdown field extraction
- `reqwest` + `tokio` for HTTP-based AI backends
- `serde` / `serde_json` for API request/response serialization

Rust was chosen for robustness, type safety, and raw text-parsing speed.
This is a pragmatic choice, not a firm commitment — if a different language
proves better suited for later stages (e.g., tighter integration with AI
tooling), that is open for discussion.

## Cargo.lock Discipline

`Cargo.lock` must be committed alongside every `Cargo.toml` change so
that each commit in the history builds reproducibly. During interactive
rebases that change the base, the lockfile can drift. To fix this, add
`exec` steps in the rebase todo after each commit that modifies
`Cargo.toml`:

    exec cargo generate-lockfile && git add Cargo.lock && git commit --amend --no-edit

Note: `&&` is correct here because `exec` lines run in the system
shell, not PowerShell.

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
- `src/bin/lore-rag.rs` — CLI for RAG ingest and query
- `src/bin/msgid-notes.rs` — tool to map Message-IDs → blob OIDs via Git notes
- `src/bin/test-prompt.rs` — smoke-test binary for AI backends
- `src/ai_backend.rs` — shared AI backend abstraction (API, Copilot CLI,
  Ollama, GitHub Models, Azure OpenAI)
- `src/rag_db.rs` — SQLite schema with FTS5 for the RAG email index
- `src/rag_parse.rs` — Markdown email field extraction
- `src/rag_ingest.rs` — file-based and git-backed ingestion
- `src/rag_query.rs` — FTS5 retrieval and prompt building
- `src/rag_git.rs` — `ls_tree` and `diff_tree` for git-backed ingestion
- `Cargo.toml` — dependencies and project metadata
- `sample4.md` — example converter output (a real Git mailing list patch)
