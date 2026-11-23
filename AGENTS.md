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

5. **AI-generated summaries** (active): Per-email summaries, per-thread
   summaries (updated as new replies arrive), and roll-up summaries at daily,
   weekly, and monthly granularity. The `digestive` pipeline handles this
   end-to-end: it enumerates emails from the archive, generates four AI
   summaries per email (human-readable, AI-consumable, thread-level human,
   thread-level AI), then produces daily/weekly/monthly digests as period
   boundaries are crossed.

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
  citations, with optional date-range filtering on the email path
- `rag_git`: `ls_tree` for full tree enumeration and `diff_tree` for
  incremental change detection between commits
- `date_range`: parser for human-friendly date specs (`2021-07`,
  `2w`, `today`, …) used by the query CLI's `--since`, `--until`,
  and `--last` flags
- `lore-rag` binary: CLI with `ingest` and `query` subcommands

An **AI backend** module (`ai_backend`) provides a shared abstraction
over multiple LLM backends: OpenAI-compatible API, GitHub Copilot CLI,
Ollama, GitHub Models, and Azure OpenAI. It includes retry logic with
exponential backoff for 429 rate limits, 400 Bad Request, and 5xx
server errors, retry-after header support with a cap, and empty
response detection. API errors are propagated as hard failures rather
than producing error markers in the output. The Azure backend
authenticates with an API key or, with `--azure-cli-token`, with an
access token from the Azure CLI (`azure_cli_token`); a token the
server refuses with a 401 is renewed once and the request repeated.

A `msgid-notes` tool maps Message-IDs to blob OIDs via Git notes
(stage 3 groundwork).

A **digestive pipeline** provides batch AI summarization of emails
and hierarchical digest generation:

- `summarize`: four-call summarization per email (human summary,
  AI summary, thread-level human, thread-level AI) using tuned
  prompts with project context
- `digestive`: the main pipeline module with a `Digestive` struct
  that owns all pipeline state and processes emails sequentially
  via a streaming `ls-tree` loop with day/week/month boundary
  detection
- `periodic_digest`: weekly and monthly digest generation from
  sub-period digests, with cache-aware collection that checks
  in-memory caches before falling back to the repository
- `thread_activity`: how much attention a thread has drawn as of a
  day (replies so far, participants, messages that day with a patch
  series counted as one, days), counted from its `.thread.md` tree; a
  thread is long with 20 or more replies or 8 or more messages that day
- `thread_deltas`: the per-thread ledger of cited daily delta bullets,
  `<root date-key>.thread.deltas.md`, committed with each daily digest
  and read back as the history of long threads
- `date_util`: day-level date helpers (ISO week boundaries, month
  extraction, day arithmetic)
- `fast_import`: a `FastImport` struct mirroring the `CatFile`
  pattern, encapsulating the git fast-import protocol behind a
  clean commit/checkpoint/finish API
- `digestive` binary: thin CLI shim that constructs a `Digestive`
  struct, computes the starting point via auto-resume from the
  latest digest commit, and delegates to the library

Daily digests lead with a `## Long threads` section.  Code picks up to
five long threads with a noteworthy update that day, longest first, and
the editor writes one subsection for each.  Code adds a line under the
heading of every long thread (or at the end of its in-brief bullet)
that states its size from the archive; the editor never writes it, and
its words count toward the digest's word window.  The items of the
leading long threads also see the thread's recent earlier developments
from its ledger, which are evidence for the audit but never today's
news.  Weekly and monthly digests lead with their long threads too, at
most eight and ten: the editor merges a thread's days into one
subsection and copies the count line of the sub-period digest with the
most replies, leaving out the single-day count after the semicolon.
Code rejects a draft whose count lines are not such copies, whose
leading section is missing or superfluous, or that has more threads than
the limit, and asks once more; a second failing draft is published
without the count lines that are not copies.

`scripts/daily-digest.sh` runs the pipeline end to end; it replaces the
hourly "Daily digest" GitHub workflow of the lore-git-md repository,
which no longer runs on a schedule.  One cycle clones whichever of the
two bare repositories is missing, downloads the executables of the
latest release of this repository, fetches both repositories, runs
`update-lore-git-md` and `digestive`, and pushes the corpus's `main` and
`refs/notes/msgid` together.  It needs `git` and `curl` (plus `tar` on
Linux or `unzip` on Windows), runs on Linux x86-64 and on Windows x86-64
and ARM64, and expects `AZURE_OPENAI_ENDPOINT` in the environment and a
completed `az login`: the Azure access tokens come from the Azure CLI.
It is meant to be run occasionally, for example on Windows in a Git for
Windows shell inside tmux.

The work directory (`LORE_GIT_MD_WORK_DIR`, by default the current
directory; created if missing, and the script changes into it) holds
`lore-git.git`, a mirror of `https://lore.kernel.org/git/1` that is
never shallow; `lore-git-md.git`, the corpus, cloned with
`--filter=blob:none`; and `helper/`, with one directory of executables
per release.  Without an option, the script runs cycles forever, each
starting at least an hour after the previous one did; a failed cycle is
reported and the next one still follows.

- `--once` runs a single cycle.  The loop runs each cycle as a process
  of its own, because the shell ignores `set -e` in a function that is
  called like `cycle || ...`.
- `--start-in-tmux` runs the script in a new, detached window named
  `daily-digest` of the tmux session, unless such a window with a live
  pane exists, and fails if there is no tmux session.  Variables set in
  the environment of the tmux server reach the window, so
  `AZURE_OPENAI_ENDPOINT` belongs there; the script passes on its own
  work directory.
- `--register-task` (Git for Windows shells only) registers the
  scheduled task `lore-git-md-daily-digest`, which runs the script with
  `--start-in-tmux` every hour.  In Windows PowerShell, it is removed
  with `Unregister-ScheduledTask -TaskName lore-git-md-daily-digest
  -Confirm:$false`.

Prompt files in `prompts/` define the AI agent behavior:
- `git-digest-email.md`: per-email summarization with taxonomy
- `git-thread-summary.md`: thread-level summary updates
- `git-project-context.md`: shared project context for all agents
- `git-daily-digest.md`: daily digest with thread-delta approach
- `git-periodic-digest.md`: weekly/monthly digest with inventory-
  first editorial workflow and include-all-topics guidance

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
The digestive pipeline has unit tests for date utilities, work-item
planning, email context loading, daily digest input construction, and
a comprehensive integration test that validates the full pipeline
end-to-end with a mock backend.
Run them before and after any change.

## Pre-push CI parity (mandatory, not optional)

Every push to a topic branch will run `.github/workflows/ci.yml`,
which executes exactly:

```sh
cargo fmt --check
cargo clippy --features test-support -- -D warnings
cargo build   --features test-support
cargo test    --features test-support
```

**Run all four locally, in that order, before every `git push`.**
Pushing without a local pass wastes a CI round-trip and the
maintainer's time; both are far more expensive than the 30 seconds
these commands take on a warm build cache.  The commands are not
individually negotiable: `cargo test` alone will let a `cargo fmt`
diff or a clippy warning slip through and fail CI.

If any of the four fails, fix it locally and re-run the whole
sequence.  Do not push "and let CI find the rest"; that pattern
compounds round-trips.

The same rule applies before force-pushing an amended or rebased
topic branch: run the full sequence again after any commit
rewrite that could have changed file contents.

## Key Files

- `src/lib.rs` — converter library (block parser + renderer + tests)
- `src/bin/mbox2md.rs` — CLI entry point for mbox-to-markdown conversion
- `src/bin/digestive.rs` — CLI for batch email summarization and digest
  generation (thin shim over `Digestive` struct)
- `src/bin/lore-rag.rs` — CLI for RAG ingest and query
- `src/bin/msgid-notes.rs` — tool to map Message-IDs → blob OIDs via Git notes
- `src/bin/test-prompt.rs` — smoke-test binary for AI backends
- `src/digestive.rs` — main digestive pipeline: work-item iterator,
  `Digestive` struct, email summarization, daily digest generation
- `src/periodic_digest.rs` — weekly and monthly digest generation from
  sub-period digests, with cache-aware collection
- `src/summarize.rs` — per-email summarization (four AI calls per email)
- `src/thread_activity.rs` — thread size counts and the long-thread
  thresholds
- `src/thread_deltas.rs` — per-thread ledger of daily delta bullets
- `src/date_util.rs` — day-level date helpers (ISO weeks, months, arithmetic)
- `src/fast_import.rs` — `FastImport` struct for git fast-import protocol
- `src/ai_backend.rs` — shared AI backend abstraction (API, Copilot CLI,
  Ollama, GitHub Models, Azure OpenAI, Mock for testing)
- `src/azure_cli_token.rs` — Azure CLI access tokens for the Azure
  backend, kept until shortly before they expire
- `src/git_util.rs` — Git helpers: source_commit_from_ref, resolve_ref,
  latest_trailer, latest_digest, last_digest_day
- `src/cat_file.rs` — `CatFile` struct for git cat-file batch protocol
- `src/rag_db.rs` — SQLite schema with FTS5 for the RAG email index
- `src/rag_parse.rs` — Markdown email field extraction
- `src/rag_ingest.rs` — file-based and git-backed ingestion
- `src/rag_query.rs` — FTS5 retrieval and prompt building
- `src/rag_git.rs` — `ls_tree` and `diff_tree` for git-backed ingestion
- `src/date_range.rs` — human-friendly date-spec parser for the
  query CLI's `--since` / `--until` / `--last` flags
- `prompts/git-digest-email.md` — email summarization prompt
- `prompts/git-thread-summary.md` — thread summary prompt
- `prompts/git-project-context.md` — shared project context
- `prompts/git-daily-digest.md` — daily digest prompt
- `prompts/git-periodic-digest.md` — weekly/monthly digest prompt
- `scripts/daily-digest.sh` — runs cycles of `update-lore-git-md` and
  `digestive`: clones, downloads the helper, fetches, pushes; options for
  tmux and a Windows scheduled task
- `Cargo.toml` — dependencies and project metadata
- `sample4.md` — example converter output (a real Git mailing list patch)
