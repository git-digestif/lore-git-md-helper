//! Batch AI summarization of emails in a bare git repository.
//!
//! Streams ls-tree output, looks up thread context lazily, generates
//! summaries via an AI backend, and writes results back to the
//! repository via fast-import.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;

use crate::ai_backend::{self, Backend};
use crate::cached_reader::CachedReader;
use crate::cat_file::{BlobRead, CatFile};
use crate::date_util::{add_days, iso_sunday, month_of};
use crate::fast_import::FastImport;
use crate::git_util::{self, latest_digest, resolve_ref, source_commit_from_ref};
use crate::periodic_digest::{Granularity, SubDigest, generate_periodic_digest};
use crate::rag_parse;
use crate::summarize::{self, EmailContext};
use crate::thread_file::{self, ThreadTree};
use crate::wc_parse::{self, WcTopic};

/// An email in the date range, with its thread root and summary status.
pub struct EmailToSummarize {
    pub dk: String,
    pub root_dk: String,
    /// Date-key of the direct parent email (`None` for thread roots).
    pub parent_dk: Option<String>,
}

/// Lazily resolve the thread root for a date-key.
///
/// Checks `thread_roots` cache first.  On miss, loads the `.thread.md`
/// file via CatFile (which follows symlinks for replies) and parses
/// it to extract the root date-key.  Falls back to `dk` itself if no
/// thread file exists (standalone email).
///
/// When loading the thread file, also caches the full `ThreadTree`
/// (keyed by root_dk) so that callers can look up per-email parent
/// relationships without a second load.
fn resolve_thread_root(
    dk: &str,
    cached: &mut impl BlobRead,
    git_ref: &str,
    thread_roots: &mut HashMap<String, String>,
    thread_trees: &mut HashMap<String, ThreadTree>,
) -> String {
    if let Some(root) = thread_roots.get(dk) {
        return root.clone();
    }
    let (root_dk, tree) = thread_file::load_from_repo(cached, git_ref, dk)
        .unwrap_or_else(|| (dk.to_string(), ThreadTree::new()));
    thread_roots.insert(dk.to_string(), root_dk.clone());
    thread_trees.entry(root_dk.clone()).or_insert(tree);
    root_dk
}

/// Find the commit OID of a daily digest by grepping the commit subject.
/// Matches both the original `digestive: daily digest for <day>` shape
/// and the corrective `digestive: redo daily digest for <day>` shape;
/// with `--date-order -1`, the newest one wins, so a subsequent
/// `redo-daily` on the following day picks up the freshly-corrected
/// tree as its "before" state rather than the original poisoned one.
fn find_digest_commit(repo_path: &str, refname: &str, day: &str) -> Option<String> {
    let needle = format!("daily digest for {day}$");
    let s = git_util::git(
        repo_path,
        &[
            "log",
            "--date-order",
            &format!("--grep={needle}"),
            "-1",
            "--format=%H",
            refname,
        ],
    )
    .ok()?;
    if s.is_empty() { None } else { Some(s) }
}

/// Whether the per-email summary was produced by the AI backend or
/// is a mechanical stub generated locally because the backend
/// refused to summarize the email (typically content-filter
/// rejection).  Drives whether `flush_batch` writes the thread
/// summary files: stubs only write the per-email `.summary.md` and
/// `.ai.md` so the email becomes visible in downstream digests, but
/// they do not pollute the cumulative `.thread.summary.md` and
/// `.thread.ai.md` (those keep their last real AI-generated state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    /// Normal: all four summaries produced by the AI backend.
    AiGenerated,
    /// Mechanical stub assembled from the email Markdown itself.
    /// `thread_human` / `thread_ai` are placeholders that
    /// `flush_batch` must not write.
    Stub,
}

/// Summary artifacts for a single email.
pub struct SummaryFiles {
    pub dk: String,
    pub root_dk: String,
    pub human: String,
    pub ai: String,
    pub thread_human: String,
    pub thread_ai: String,
    pub kind: SummaryKind,
}

/// Build the (path, content) refs that `flush_batch` commits for a
/// batch of summarized emails.  Per-email `.human.md` and `.ai.md`
/// are always emitted; the cumulative `.thread.human.md` and
/// `.thread.ai.md` are emitted only for `SummaryKind::AiGenerated`
/// so that stubs do not overwrite real thread state.
///
/// Extracted from `flush_batch` so a unit test can assert the
/// "stubs never write thread files" invariant without round-tripping
/// through fast-import.
fn summary_files_to_refs(batch: &[SummaryFiles]) -> Vec<(String, &str)> {
    batch
        .iter()
        .flat_map(|sf| {
            let per_email = [
                (format!("{}.human.md", sf.dk), sf.human.as_str()),
                (format!("{}.ai.md", sf.dk), sf.ai.as_str()),
            ];
            let thread = match sf.kind {
                SummaryKind::Stub => Vec::new(),
                SummaryKind::AiGenerated => vec![
                    (
                        format!("{}.thread.human.md", sf.root_dk),
                        sf.thread_human.as_str(),
                    ),
                    (
                        format!("{}.thread.ai.md", sf.root_dk),
                        sf.thread_ai.as_str(),
                    ),
                ],
            };
            per_email.into_iter().chain(thread)
        })
        .collect()
}

/// Compute the path of the content-filter-rejections tracking file
/// for a given email date-key (`YYYY/MM/DD/HH-MM-SS`).  One file per
/// calendar day at `content-filter-rejections/YYYY/MM/DD.md`, keeping
/// the tracking file proportionate to the number of rejections on
/// that day rather than letting a single monolithic file grow
/// unboundedly across years.
fn rejection_tracking_path(dk: &str) -> Option<String> {
    // Expect "YYYY/MM/DD/HH-MM-SS"; take the first three path parts.
    let mut parts = dk.split('/');
    let y = parts.next()?;
    let m = parts.next()?;
    let d = parts.next()?;
    if y.is_empty() || m.is_empty() || d.is_empty() {
        return None;
    }
    Some(format!("content-filter-rejections/{y}/{m}/{d}.md"))
}

/// Build a `RejectionEntry` from the email Markdown.  Extracts
/// subject, author, and Message-ID via `rag_parse::parse_email` so
/// the bullet format stays consistent with the stub summary fields.
/// Returns `None` if `dk` is malformed (we cannot link to the email
/// without a usable date-key).
fn make_rejection_entry(dk: &str, email_md: &str) -> Option<RejectionEntry> {
    let parsed = rag_parse::parse_email(email_md);
    let subject = if parsed.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        parsed.subject
    };
    let author = if parsed.author.is_empty() {
        "(unknown author)".to_string()
    } else {
        parsed.author
    };
    // Relative link from the tracking file (3 levels deep:
    // content-filter-rejections/YYYY/MM/DD.md) up to the email
    // (YYYY/MM/DD/HH-MM-SS.md, also 3 levels under repo root).
    let email_link = format!("../../../{dk}.md");
    let msgid_line = if parsed.message_id.is_empty() {
        String::new()
    } else {
        format!(
            "\n  - Message-ID: [`{0}`](https://lore.kernel.org/git/{0})",
            parsed.message_id,
        )
    };
    let time = dk.rsplit('/').next().unwrap_or(dk);
    let bullet = format!("- **{time}** [{subject}]({email_link}) by *{author}*{msgid_line}\n");
    Some(RejectionEntry {
        dk: dk.to_string(),
        bullet,
    })
}

/// Merge a list of new rejection entries into the existing tracking
/// file content (or build a fresh file if `existing` is `None`).
/// Entries whose `dk` already appears in `existing` are skipped, so
/// reruns of the same date range never produce duplicate bullets.
fn merge_rejection_file(
    date_label: &str,
    existing: Option<&str>,
    new: &[RejectionEntry],
) -> String {
    let header = format!(
        "# Content-filter rejections for {date_label}\n\n\
         Emails whose AI summarization was rejected by the backend's \
         content filter on this day.  The per-email `.summary.md` and \
         `.ai.md` for these are mechanical stubs (subject, author, \
         opening cover-letter paragraph) rather than real AI summaries; \
         follow the link to read the full message.\n\n"
    );
    let existing_body = existing
        .map(|s| s.trim_start_matches(&header[..]).to_string())
        .unwrap_or_default();
    let mut additions = String::new();
    for entry in new {
        // Skip if the existing file already contains this exact dk;
        // we recognize a dk by the leading "- **HH-MM-SS** " and by
        // the email link "../../../DK.md", but the email-link check
        // alone is the precise dedup signal.
        let link_marker = format!("({})", format_args!("../../../{}.md", entry.dk));
        if existing_body.contains(&link_marker) {
            continue;
        }
        additions.push_str(&entry.bullet);
    }
    if additions.is_empty() && existing.is_some() {
        // No new entries to add and the file is already present;
        // returning the existing content unchanged means the
        // fast-import write will be a no-op blob (same OID).
        return format!("{header}{existing_body}");
    }
    format!("{header}{existing_body}{additions}")
}

///
/// Thread AI summaries are looked up via the `BlobRead` implementation,
/// which may be a `CachedReader` (checking an in-memory cache first,
/// then falling through to git) or a `MockBlobs` for tests.
/// Returns `None` if the email `.md` blob is missing.
pub fn load_email_context(
    email: &EmailToSummarize,
    cat: &mut impl BlobRead,
    git_ref: &str,
) -> Option<EmailContext> {
    let email_spec = format!("{git_ref}:{}.md", email.dk);
    let email_md = cat.get_str(&email_spec)?;

    let thread_spec = format!("{git_ref}:{}.thread.ai.md", email.root_dk);
    let thread_ai_summary = cat.get_str(&thread_spec);

    let parent_ai_summary = email.parent_dk.as_ref().and_then(|parent_dk| {
        let spec = format!("{git_ref}:{parent_dk}.ai.md");
        cat.get_str(&spec)
    });

    // From: line of the email itself (drives self-attribution of any
    // unquoted stretch following a quote block).
    let from = rag_parse::parse_email(&email_md).author;
    let from = if from.is_empty() { None } else { Some(from) };

    // From: line of the parent email (drives first-level quote-block
    // attribution).  Only relevant for replies.
    let parent_from = email.parent_dk.as_ref().and_then(|parent_dk| {
        let spec = format!("{git_ref}:{parent_dk}.md");
        let md = cat.get_str(&spec)?;
        let a = rag_parse::parse_email(&md).author;
        if a.is_empty() { None } else { Some(a) }
    });

    Some(EmailContext {
        email_md,
        thread_ai_summary,
        parent_ai_summary,
        from,
        parent_from,
    })
}

/// Summarize a single email, loading its content and thread context
/// from the repo.
///
/// The caller is responsible for updating the CachedReader with the
/// new thread AI summary after a successful summarization.
pub async fn summarize_one(
    email: &EmailToSummarize,
    cat: &mut impl BlobRead,
    backend: &Backend,
    git_ref: &str,
    label: Option<&str>,
) -> Result<Option<SummaryFiles>> {
    let ctx = match load_email_context(email, cat, git_ref) {
        Some(ctx) => ctx,
        None => {
            eprintln!("[warn] {}.md not found, skipping", email.dk);
            return Ok(None);
        }
    };

    eprintln!(
        "[digestive] {} {} ...",
        label.unwrap_or("summarizing"),
        email.dk
    );
    let result = match summarize::summarize_email(&ctx, backend).await {
        Ok(r) => r,
        Err(e) if ai_backend::is_no_choices(&e) => {
            // The backend accepted the request but returned no
            // choices (typically: prompt exceeded the model context
            // window).  Skip this one email rather than aborting the
            // whole pipeline run; the diagnostic chain printed below
            // preserves the body snippet so a later run or human can
            // investigate.
            eprintln!(
                "[warn] {}.md: backend returned no choices, skipping ({:#})",
                email.dk, e
            );
            return Ok(None);
        }
        Err(e) if ai_backend::is_content_filter(&e) => {
            // Azure's Responsible AI content filter classifies the
            // request as disallowed and returns either a 400 with
            // `"code":"content_filter"` or a 200 with empty content
            // and `finish_reason: content_filter`.  Either way the
            // rejection is deterministic; retrying burns ~5x the
            // tokens for nothing.  Generate a mechanical stub from
            // the email Markdown itself so the email is still
            // visible in downstream digests (subject, author, the
            // opening of the cover letter), while a [warn] line
            // makes the substitution discoverable in CI logs.
            eprintln!(
                "[warn] {}.md: backend content-filter rejected request, \
                 generating mechanical stub ({:#})",
                email.dk, e
            );
            let (human, ai) =
                summarize::stub_summary_from_md(&ctx.email_md, "the backend content filter");
            return Ok(Some(SummaryFiles {
                dk: email.dk.clone(),
                root_dk: email.root_dk.clone(),
                human,
                ai,
                // Placeholders that flush_batch will skip writing for
                // SummaryKind::Stub; left non-empty defensively so
                // that any future code reading these fields without
                // checking `kind` sees a clearly-marked stub rather
                // than the empty string.
                thread_human: String::from("<!-- stub: thread summary not updated -->\n"),
                thread_ai: String::from("<!-- stub: thread summary not updated -->\n"),
                kind: SummaryKind::Stub,
            }));
        }
        Err(e) => return Err(e),
    };

    Ok(Some(SummaryFiles {
        dk: email.dk.clone(),
        root_dk: email.root_dk.clone(),
        human: result.human_summary,
        ai: result.ai_summary,
        thread_human: result.thread_human_summary,
        thread_ai: result.thread_ai_summary,
        kind: SummaryKind::AiGenerated,
    }))
}

/// Result of a pipeline run.
pub struct PipelineResult {
    pub total_processed: u64,
}

/// Per-day mutable state tracked across day boundaries in the
/// main loop.  Reset (partially) at each day/week/month boundary.
#[derive(Default)]
struct LoopState {
    prev_day: Option<String>,
    prev_week: Option<String>,
    prev_month: Option<String>,
    day_has_digest: bool,
    week_has_digest: bool,
    month_has_digest: bool,
    /// Whether the current week/month has any content that could
    /// feed digest generation.
    week_has_content: bool,
    month_has_content: bool,
    /// AI summary existence for emails in the current day.
    day_ai_exists: HashSet<String>,
    /// All email datekeys for the current day.
    day_email_dks: Vec<String>,
    /// Lazy thread root cache: dk → root_dk.
    thread_roots: HashMap<String, String>,
    /// Thread tree cache: root_dk → ThreadTree.
    thread_trees: HashMap<String, ThreadTree>,
}

/// Pipeline state for the digestive batch processor.
///
/// Collects all shared mutable state for email summarization,
/// daily digest generation, and periodic (weekly/monthly) digest
/// generation.
pub struct Digestive<'a> {
    repo_path: &'a str,
    git_ref: &'a str,
    cached: CachedReader,
    fi: Option<FastImport>,
    source_commit: Option<String>,
    backend: Option<&'a Backend>,
    dry_run: bool,
    batch_size: usize,
    /// Days (e.g. "2025/01/02") with a daily `digest.ai.md`, seen in
    /// ls-tree or generated this run.  Content is read via CachedReader.
    daily_digest_days: std::collections::BTreeSet<String>,
    /// Sundays (e.g. "2025/01/05") with a weekly `digest.weekly.ai.md`.
    weekly_digest_sundays: std::collections::BTreeSet<String>,
    /// Resolved OID of the commit whose tree represents the "before
    /// today" thread state for daily digest generation.  Cleared after
    /// each daily digest commit so that the next day re-resolves it
    /// (via polling) once fast-import's checkpoint has landed.
    before_oid: Option<String>,
    /// Day string (e.g. "2025/01/13") of the most recently written
    /// daily digest commit.  Used by `resolve_before_oid()` to poll
    /// for the commit OID when `before_oid` is `None`.
    last_digested_day: Option<String>,
    total_processed: u64,
    day_summaries: Vec<(String, String, String)>,
    batch: Vec<SummaryFiles>,
    /// Pending content-filter rejection entries, keyed by tracking
    /// path ("content-filter-rejections/YYYY/MM/DD.md").  Drained by
    /// `flush_batch`, which merges them into the existing file (or
    /// creates one) and includes the result in the same fast-import
    /// commit as the per-email stub summaries.
    pending_rejections: HashMap<String, Vec<RejectionEntry>>,
    /// Wall-clock deadline.  When `Some` and `Instant::now()` has
    /// passed this value, the streaming loop in `run` stops at the
    /// next iteration boundary rather than initiating another AI
    /// call.  Set from the `--max-runtime` CLI argument.
    deadline: Option<std::time::Instant>,
}

/// A single content-filter rejection bullet enqueued for inclusion
/// in the tracking file at `content-filter-rejections/YYYY/MM/DD.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RejectionEntry {
    /// Date-key of the rejected email (e.g. `2026/06/10/14-57-08`),
    /// used for dedup against the existing tracking file content.
    dk: String,
    /// Rendered Markdown bullet line, starting with `- ` and ending
    /// with a single newline.  Includes the subject (linked to the
    /// email's `.md` blob in the same target repo), the author, and
    /// a `Message-ID:` sub-bullet linking to lore.kernel.org.
    bullet: String,
}

impl<'a> Digestive<'a> {
    pub fn new(
        repo_path: &'a str,
        git_ref: &'a str,
        batch_size: usize,
        backend: Option<&'a Backend>,
        dry_run: bool,
    ) -> Result<Self> {
        let cat = CatFile::new(repo_path).context("failed to open target repo")?;
        let cached = CachedReader::new(cat);

        let fi = if !dry_run {
            let mut fi = FastImport::new(repo_path, git_ref)?;
            if let Some(oid) = resolve_ref(repo_path, git_ref) {
                fi.set_parent(oid);
            }
            Some(fi)
        } else {
            None
        };

        let source_commit = source_commit_from_ref(repo_path, git_ref);

        let (last_digested_day, before_oid) = match latest_digest(repo_path, git_ref) {
            Some((day, oid)) => (Some(day), Some(oid)),
            None => (None, resolve_ref(repo_path, git_ref)),
        };

        Ok(Digestive {
            repo_path,
            git_ref,
            cached,
            fi,
            source_commit,
            backend,
            dry_run,
            batch_size,
            daily_digest_days: std::collections::BTreeSet::new(),
            weekly_digest_sundays: std::collections::BTreeSet::new(),
            before_oid,
            last_digested_day,
            total_processed: 0,
            day_summaries: Vec::new(),
            batch: Vec::new(),
            pending_rejections: HashMap::new(),
            deadline: None,
        })
    }

    /// Install a wall-clock deadline.  When `Instant::now()` has
    /// passed `deadline`, the streaming loop in `run` exits cleanly
    /// at the next iteration boundary.  The midnight flush at the end
    /// of `run` is also suppressed in that case, so a partially
    /// processed day does not get a partial daily digest committed.
    pub fn with_deadline(mut self, deadline: std::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    fn deadline_exceeded(&self) -> bool {
        self.deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
    }

    /// Summarize a single email: call the AI backend, update caches,
    /// and add the result to the current batch.
    ///
    /// Returns `true` if a summary was produced (even if it contained
    /// errors), `false` if the email was skipped (dry-run or missing).
    async fn summarize_and_record(
        &mut self,
        dk: &str,
        root_dk: &str,
        parent_dk: Option<&str>,
        label: Option<&str>,
    ) -> Result<bool> {
        let email = EmailToSummarize {
            dk: dk.to_string(),
            root_dk: root_dk.to_string(),
            parent_dk: parent_dk.map(|s| s.to_string()),
        };
        if self.dry_run {
            eprintln!("[dry-run] would summarize {dk}");
            return Ok(false);
        }
        let sf = match summarize_one(
            &email,
            &mut self.cached,
            self.backend.context("backend unavailable")?,
            self.git_ref,
            label,
        )
        .await?
        {
            Some(sf) => sf,
            None => return Ok(false),
        };
        // Cache the AI summaries so that backfill_thread (and the
        // main loop) can see them without waiting for a fast-import
        // checkpoint to land.  For stub summaries we cache the
        // per-email `.ai.md` (so replies see at least subject/author
        // for parent context) but never the thread summary (which
        // would otherwise overwrite a real AI-generated thread state
        // with a placeholder).
        let ai_key = format!("{}:{}.ai.md", self.git_ref, sf.dk);
        self.cached.insert(ai_key, sf.ai.clone());
        let cache_thread_ai = match sf.kind {
            // Old data may contain <!-- ERROR markers from earlier
            // runs; avoid caching those as valid thread summaries.
            SummaryKind::AiGenerated => !sf.thread_ai.starts_with("<!-- ERROR"),
            SummaryKind::Stub => false,
        };
        if cache_thread_ai {
            let key = format!("{}:{}.thread.ai.md", self.git_ref, sf.root_dk,);
            self.cached.insert(key, sf.thread_ai.clone());
        }
        self.day_summaries
            .push((sf.dk.clone(), sf.root_dk.clone(), sf.ai.clone()));
        if sf.kind == SummaryKind::Stub {
            // Load the email Markdown for the tracking-file bullet.
            // Re-reading via the cached reader is cheap: the same
            // blob was just read by `summarize_one`.
            let spec = format!("{}:{}.md", self.git_ref, sf.dk);
            if let Some(email_md) = self.cached.get_str(&spec)
                && let Some(path) = rejection_tracking_path(&sf.dk)
                && let Some(entry) = make_rejection_entry(&sf.dk, &email_md)
            {
                self.pending_rejections.entry(path).or_default().push(entry);
            }
        }
        self.batch.push(sf);
        self.total_processed += 1;
        if self.batch.len() >= self.batch_size {
            self.flush_batch()?;
        }
        Ok(true)
    }

    /// Backfill unsummarized emails in a thread before processing a reply.
    ///
    /// When a reply arrives for a thread whose earlier emails lack
    /// `.ai.md` summaries, this method loads the thread tree, collects
    /// all participating date-keys strictly before `up_to_dk`, and
    /// summarizes any that are missing in chronological order. This
    /// ensures the thread AI summary accumulates correctly before the
    /// new reply is processed.
    async fn backfill_thread(&mut self, root_dk: &str, up_to_dk: &str) -> Result<()> {
        let (_, tree) = match thread_file::load_from_repo(&mut self.cached, self.git_ref, root_dk) {
            Some(pair) => pair,
            None => return Ok(()),
        };

        let mut dks: Vec<String> = tree
            .date_keys()
            .filter(|d| *d < up_to_dk)
            .map(|s| s.to_string())
            .collect();
        dks.sort();

        for dk in &dks {
            let spec = format!("{}:{dk}.ai.md", self.git_ref);
            if self.cached.get_str(&spec).is_some() {
                continue; // already summarized
            }
            let parent_dk = tree.parent_of(dk);
            if let Err(e) = self
                .summarize_and_record(dk, root_dk, parent_dk, Some("backfilling"))
                .await
            {
                let _ = self.flush_batch();
                return Err(e);
            }
        }

        Ok(())
    }

    fn flush_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() || self.dry_run {
            return Ok(());
        }

        let last_dk = &self.batch.last().unwrap().dk;
        let mut msg = format!(
            "digestive: summarize {} email(s)\n\nDigestive-Progress: {last_dk}",
            self.batch.len(),
        );
        if let Some(ref sc) = self.source_commit {
            msg.push_str(&format!("\nSource-Commit: {sc}"));
        }
        let files = summary_files_to_refs(&self.batch);

        // Merge any pending content-filter rejections into their
        // per-day tracking files.  We read the existing file via the
        // CachedReader (which transparently falls through to git
        // cat-file) and produce merged content that the same
        // fast-import commit will write back.  The merge dedupes
        // against any bullet whose email link already appears in
        // the existing content, so reruns don't accumulate
        // duplicate entries.
        let pending = std::mem::take(&mut self.pending_rejections);
        let mut tracking_files: Vec<(String, String)> = Vec::with_capacity(pending.len());
        for (path, entries) in pending {
            let date_label = path
                .strip_prefix("content-filter-rejections/")
                .and_then(|s| s.strip_suffix(".md"))
                .map(|s| s.replace('/', "-"))
                .unwrap_or_else(|| path.clone());
            let spec = format!("{}:{path}", self.git_ref);
            let existing = self.cached.get_str(&spec);
            let merged = merge_rejection_file(&date_label, existing.as_deref(), &entries);
            // Update the cache so subsequent rejections on the same
            // day (within this run, before the fast-import commit
            // lands) see the merged content.
            self.cached.insert(spec, merged.clone());
            tracking_files.push((path, merged));
        }

        let mut refs: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        for (p, c) in &tracking_files {
            refs.push((p.as_str(), c.as_str()));
        }
        self.fi
            .as_mut()
            .context("fast-import unavailable")?
            .commit(&msg, &refs)?;
        self.batch.clear();

        Ok(())
    }

    /// Resolve the "before" commit OID for daily digest generation.
    ///
    /// When `before_oid` is already cached, returns it immediately.
    /// Otherwise, polls `git log --grep` for the commit that wrote
    /// the previous daily digest (identified by `last_digested_day`).
    /// This handles the case where fast-import's checkpoint hasn't
    /// landed yet: we try immediately, then retry with exponential
    /// backoff (100ms, 200ms, 400ms, ... up to ~25s total).
    ///
    /// Returns an empty string as fallback (no prior state).
    fn resolve_before_oid(&mut self) -> String {
        if let Some(ref oid) = self.before_oid {
            return oid.clone();
        }
        let day = match self.last_digested_day {
            Some(ref d) => d.clone(),
            None => return String::new(),
        };
        let mut delay_ms = 100u64;
        for attempt in 0..10 {
            if let Some(oid) = find_digest_commit(self.repo_path, self.git_ref, &day) {
                self.before_oid = Some(oid.clone());
                return oid;
            }
            eprintln!(
                "[digestive] waiting for daily digest commit \
                 for {day} to land (attempt {}/{}, {}ms)...",
                attempt + 1,
                10,
                delay_ms,
            );
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            delay_ms = (delay_ms * 2).min(10_000);
        }
        eprintln!(
            "[warn] daily digest commit for {day} not found after \
             polling; falling back to ref tip",
        );
        resolve_ref(self.repo_path, self.git_ref).unwrap_or_default()
    }

    async fn commit_day_digest(&mut self, day: &str) -> Result<()> {
        let ai_path = format!("{day}/digest.ai.md");
        let exists = self
            .cached
            .get_str(&format!("{}:{ai_path}", self.git_ref))
            .is_some();
        if exists {
            return Ok(());
        }

        if self.dry_run {
            eprintln!("[dry-run] would generate daily digest for {day}");
            return Ok(());
        }

        let before = self.resolve_before_oid();
        let (threads, email_count) =
            build_day_digest_input(&self.day_summaries, &before, self.git_ref, &mut self.cached);
        let digest = generate_daily_digest(
            day,
            &threads,
            email_count,
            self.backend.context("backend unavailable")?,
        )
        .await?;

        self.daily_digest_days.insert(day.to_string());
        self.cached.insert(
            format!("{}:{day}/digest.human.md", self.git_ref),
            digest.human.clone(),
        );
        self.cached.insert(
            format!("{}:{day}/digest.ai.md", self.git_ref),
            digest.ai.clone(),
        );

        let mut msg = format!("digestive: daily digest for {day}");
        if let Some(ref sc) = self.source_commit {
            msg.push_str(&format!("\n\nSource-Commit: {sc}"));
        }
        let human_path = format!("{day}/digest.human.md");
        let ai_path = format!("{day}/digest.ai.md");
        let files = [
            (human_path.as_str(), digest.human.as_str()),
            (ai_path.as_str(), digest.ai.as_str()),
        ];
        let fi = self.fi.as_mut().context("fast-import unavailable")?;
        fi.commit(&msg, &files)?;
        fi.checkpoint()?;

        // Clear before_oid so the next day re-resolves it via polling,
        // waiting for this commit's checkpoint to land.
        self.last_digested_day = Some(day.to_string());
        self.before_oid = None;

        Ok(())
    }

    /// Called at each day boundary (and implicitly, never for the last
    /// day).  Emits daily, weekly, and monthly digests for completed
    /// periods.
    async fn finalize_day(
        &mut self,
        state: &mut LoopState,
        new_day: &str,
        since: Option<&str>,
    ) -> Result<()> {
        let Some(ref prev) = state.prev_day else {
            return Ok(());
        };

        // Skip digest generation for days entirely before --since.
        let before_since = since.is_some_and(|s| prev.as_str() < s);
        if before_since {
            self.day_summaries.clear();
            return Ok(());
        }

        // --- Daily digest for the previous day ---
        if !state.day_has_digest {
            for dk in state.day_email_dks.iter() {
                let already_loaded = self.day_summaries.iter().any(|(d, _, _)| d == dk);
                if state.day_ai_exists.contains(dk.as_str()) && !already_loaded {
                    let spec = format!("{}:{dk}.ai.md", self.git_ref);
                    if let Some(ai_text) = self.cached.get_str(&spec) {
                        let root_dk = resolve_thread_root(
                            dk,
                            &mut self.cached,
                            self.git_ref,
                            &mut state.thread_roots,
                            &mut state.thread_trees,
                        );
                        self.day_summaries.push((dk.clone(), root_dk, ai_text));
                    }
                }
            }

            if !self.day_summaries.is_empty() {
                self.flush_batch()?;
                self.commit_day_digest(prev).await?;
            }
        }
        self.day_summaries.clear();

        // --- Weekly digest at week boundary ---
        let new_week = iso_sunday(new_day);
        if new_week != state.prev_week
            && let Some(ref pw) = state.prev_week
            && !state.week_has_digest
            && state.week_has_content
        {
            self.commit_weekly_digest(pw).await?;
        }

        // --- Monthly digest at month boundary ---
        let new_month = month_of(new_day);
        if Some(new_month) != state.prev_month.as_deref()
            && let Some(ref pm) = state.prev_month
            && !state.month_has_digest
            && state.month_has_content
        {
            self.commit_monthly_digest(pm).await?;
        }

        Ok(())
    }

    async fn commit_weekly_digest(&mut self, week_sunday: &str) -> Result<()> {
        let ai_path = format!("{week_sunday}/digest.weekly.ai.md");
        let exists = self
            .cached
            .get_str(&format!("{}:{ai_path}", self.git_ref))
            .is_some();
        if exists {
            return Ok(());
        }

        if self.dry_run {
            eprintln!("[dry-run] would generate weekly digest for {week_sunday}");
            return Ok(());
        }

        let monday = add_days(week_sunday, -6).context("invalid week sunday")?;

        // Collect daily digests from the precomputed set, reading
        // content via CachedReader (covers both pre-existing and
        // just-generated digests without spawning ls-tree).
        let digests: Vec<_> = self
            .daily_digest_days
            .range(monday.clone()..=week_sunday.to_string())
            .filter_map(|day| {
                let spec = format!("{}:{day}/digest.human.md", self.git_ref);
                self.cached.get_str(&spec).map(|content| SubDigest {
                    label: day.clone(),
                    content,
                })
            })
            .collect();
        if digests.is_empty() {
            eprintln!(
                "[digestive] no daily digests for {monday}..{week_sunday}, \
                skipping weekly digest"
            );
            return Ok(());
        }

        let label = format!("{monday} -- {week_sunday}");
        let result = generate_periodic_digest(
            &label,
            Granularity::Weekly,
            &digests,
            self.backend.context("backend unavailable")?,
        )
        .await?;

        self.weekly_digest_sundays.insert(week_sunday.to_string());
        self.cached.insert(
            format!("{}:{week_sunday}/digest.weekly.human.md", self.git_ref),
            result.human.clone(),
        );
        self.cached.insert(
            format!("{}:{week_sunday}/digest.weekly.ai.md", self.git_ref),
            result.ai.clone(),
        );

        let ai_path = format!("{week_sunday}/digest.weekly.ai.md");
        let human_path = format!("{week_sunday}/digest.weekly.human.md");
        let mut msg = format!("digestive: weekly digest for {week_sunday}");
        if let Some(ref sc) = self.source_commit {
            msg.push_str(&format!("\n\nSource-Commit: {sc}"));
        }
        let files = [
            (human_path.as_str(), result.human.as_str()),
            (ai_path.as_str(), result.ai.as_str()),
        ];
        let fi = self.fi.as_mut().context("fast-import unavailable")?;
        fi.commit(&msg, &files)?;
        fi.checkpoint()?;

        Ok(())
    }

    async fn commit_monthly_digest(&mut self, month: &str) -> Result<()> {
        let ai_path = format!("{month}/digest.monthly.ai.md");
        let exists = self
            .cached
            .get_str(&format!("{}:{ai_path}", self.git_ref))
            .is_some();
        if exists {
            return Ok(());
        }

        if self.dry_run {
            eprintln!("[dry-run] would generate monthly digest for {month}");
            return Ok(());
        }

        // Collect weekly digests from the precomputed set, reading
        // content via CachedReader.  A week overlaps this month if
        // its Monday..Sunday range intersects the month's day range.
        let month_start = format!("{month}/01");
        let month_end = format!("{month}/31");
        let digests: Vec<_> = self
            .weekly_digest_sundays
            .iter()
            .filter_map(|sunday| {
                let monday = add_days(sunday, -6)?;
                let overlaps = sunday.as_str() >= month_start.as_str()
                    && monday.as_str() <= month_end.as_str();
                if !overlaps {
                    return None;
                }
                let spec = format!("{}:{sunday}/digest.weekly.human.md", self.git_ref,);
                let content = self.cached.get_str(&spec)?;
                Some(SubDigest {
                    label: format!("{monday} -- {sunday}"),
                    content,
                })
            })
            .collect();
        if digests.is_empty() {
            eprintln!(
                "[digestive] no weekly digests for {month}, \
                skipping monthly digest"
            );
            return Ok(());
        }

        let from = format!("{month}/01");
        let to = format!("{month}/31");
        let label = format!("{from} -- {to}");
        let result = generate_periodic_digest(
            &label,
            Granularity::Monthly,
            &digests,
            self.backend.context("backend unavailable")?,
        )
        .await?;

        let ai_path = format!("{month}/digest.monthly.ai.md");
        let human_path = format!("{month}/digest.monthly.human.md");
        let mut msg = format!("digestive: monthly digest for {month}");
        if let Some(ref sc) = self.source_commit {
            msg.push_str(&format!("\n\nSource-Commit: {sc}"));
        }
        let files = [
            (human_path.as_str(), result.human.as_str()),
            (ai_path.as_str(), result.ai.as_str()),
        ];
        let fi = self.fi.as_mut().context("fast-import unavailable")?;
        fi.commit(&msg, &files)?;
        fi.checkpoint()?;

        Ok(())
    }

    pub fn finish(self) -> Result<PipelineResult> {
        if let Some(fi) = self.fi {
            fi.finish()?;
        }
        Ok(PipelineResult {
            total_processed: self.total_processed,
        })
    }

    /// Run the pipeline as a single streaming pass over `ls-tree -r`.
    ///
    /// Instead of planning all work items upfront, this processes emails
    /// inline as they are discovered and emits daily digest events at
    /// day boundaries.
    pub async fn run(&mut self, since: Option<&str>, until: Option<&str>) -> Result<()> {
        let stdout = match git_util::resolve_ref(self.repo_path, self.git_ref) {
            None => {
                eprintln!(
                    "[digestive] Nothing to do (ref {} not found).",
                    self.git_ref
                );
                return Ok(());
            }
            Some(oid) => git_util::git(self.repo_path, &["ls-tree", "-r", "--name-only", &oid])?,
        };

        let in_range = |dk: &str| -> bool {
            let before_since = since.is_some_and(|s| dk < s);
            let after_until = until.is_some_and(|u| dk >= u);
            !before_since && !after_until
        };

        // Clock-skew guard: datekeys whose day portion lexically
        // exceeds tomorrow are clamped to today.
        use crate::date_util::format_datekey;
        let now = time::OffsetDateTime::now_utc();
        let today = format_datekey(now)[..10].to_string();
        let tomorrow = format_datekey(now + time::Duration::days(1))[..10].to_string();

        // Per-day state, reset at each day boundary.
        let mut state = LoopState::default();

        for path in stdout.lines() {
            if self.deadline_exceeded() {
                eprintln!(
                    "[digestive] --max-runtime exceeded; exiting cleanly \
                     after summarizing {} email(s)",
                    self.total_processed,
                );
                break;
            }

            let raw_day = match path.get(..10) {
                Some(d) if path.as_bytes().get(10) == Some(&b'/') => d,
                _ => continue,
            };

            // Clamp bogus future dates to today.
            let day = if raw_day > tomorrow.as_str() {
                eprintln!(
                    "[digestive] clamping bogus future date \
                     {raw_day} → {today}",
                );
                today.as_str()
            } else {
                raw_day
            };

            // --- Day boundary detection ---
            if state.prev_day.as_deref() != Some(day) {
                self.finalize_day(&mut state, day, since).await?;

                let new_week = iso_sunday(day);
                let new_month = month_of(day).to_string();

                state.prev_day = Some(day.to_string());
                if new_week != state.prev_week {
                    state.week_has_digest = false;
                    state.week_has_content = false;
                    state.prev_week = new_week;
                }
                if Some(new_month.as_str()) != state.prev_month.as_deref() {
                    state.month_has_digest = false;
                    state.month_has_content = false;
                    state.prev_month = Some(new_month);
                }
                state.day_has_digest = false;
                state.day_ai_exists.clear();
                state.day_email_dks.clear();
            }

            // --- File classification ---
            if let Some(dk) = path.strip_suffix(".ai.md") {
                if dk.ends_with("/digest") {
                    state.day_has_digest = true;
                    state.week_has_content = true;
                    state.month_has_content = true;
                    self.daily_digest_days.insert(day.to_string());
                } else if dk.ends_with("/digest.weekly") {
                    state.week_has_digest = true;
                    state.month_has_content = true;
                    self.weekly_digest_sundays.insert(day.to_string());
                } else if dk.ends_with("/digest.monthly") {
                    state.month_has_digest = true;
                } else if !dk.ends_with(".thread") {
                    state.day_ai_exists.insert(dk.to_string());
                    state.week_has_content = true;
                    state.month_has_content = true;
                }
            } else if let Some(dk) = path.strip_suffix(".md") {
                // Skip derivative files.
                if dk.ends_with(".human")
                    || dk.ends_with(".thread")
                    || dk.ends_with(".thread.human")
                    || dk.ends_with(".thread.ai")
                {
                    continue;
                }

                state.day_email_dks.push(dk.to_string());

                // Summarize in-range emails that lack an AI summary.
                if in_range(dk) && !state.day_ai_exists.contains(dk) {
                    let root_dk = resolve_thread_root(
                        dk,
                        &mut self.cached,
                        self.git_ref,
                        &mut state.thread_roots,
                        &mut state.thread_trees,
                    );

                    // Backfill older thread members that lack summaries,
                    // but only up to this email's position in time.
                    if root_dk != dk {
                        self.backfill_thread(&root_dk, dk).await?;
                    }

                    // Backfill may have already summarized this email
                    // (it processes unsummarized thread members up to dk).
                    let ai_spec = format!("{}:{dk}.ai.md", self.git_ref);
                    if self.cached.get_str(&ai_spec).is_some() {
                        state.week_has_content = true;
                        state.month_has_content = true;
                        continue;
                    }

                    let parent_dk = state
                        .thread_trees
                        .get(&root_dk)
                        .and_then(|t| t.parent_of(dk));
                    if let Err(e) = self
                        .summarize_and_record(dk, &root_dk, parent_dk, None)
                        .await
                    {
                        // Flush successful work before propagating the error
                        // so we don't lose everything since the last checkpoint.
                        let _ = self.flush_batch();
                        return Err(e);
                    }
                    state.week_has_content = true;
                    state.month_has_content = true;
                }
            }
            // All other files (thread.md, thread.human.md, etc.) are skipped.
        }

        // Finalize the last day.  When UTC midnight for the last day
        // in the stream had already passed at least 15 minutes before
        // the ls-tree snapshot was taken, the day is complete and its
        // daily digest (plus any pending weekly/monthly digests for
        // completed periods) should be emitted.
        //
        // The 15-minute grace period guards against a race where the
        // email import finishes just before midnight but the pipeline
        // starts just after: without the grace period, the snapshot
        // might miss late-arriving emails that belong to the day.
        //
        // Skip this when a deadline forced an early exit: the day in
        // flight is likely partial, and emitting a digest from it
        // would commit an incomplete summary that the next run cannot
        // overwrite (commit_day_digest is gated on absence).
        if !self.deadline_exceeded() {
            let cutoff = format_datekey(now - time::Duration::minutes(15));
            let cutoff_day = &cutoff[..10];
            if state.prev_day.as_deref().is_some_and(|d| d < cutoff_day) {
                self.finalize_day(&mut state, cutoff_day, since).await?;
            }
        }
        self.flush_batch()?;
        self.day_summaries.clear();

        Ok(())
    }
}

/// Run the full summarization pipeline.
///
/// Pass `None` for `backend` in dry-run mode to skip AI calls and
/// fast-import writes.
pub async fn run_pipeline(
    repo_path: &str,
    git_ref: &str,
    since: Option<&str>,
    until: Option<&str>,
    batch_size: usize,
    backend: Option<&Backend>,
    dry_run: bool,
) -> Result<PipelineResult> {
    let mut s = Digestive::new(repo_path, git_ref, batch_size, backend, dry_run)?;
    s.run(since, until).await?;
    s.finish()
}

const DAILY_DIGEST_AGENT: &str = include_str!("../prompts/git-daily-digest.md");
const DAILY_DELTA_AGENT: &str = include_str!("../prompts/git-daily-delta.md");
const DAILY_SHORT_DELTA_AGENT: &str = include_str!("../prompts/git-daily-short-delta.md");

/// Per-thread data accumulated for a single day's digest.
pub struct ThreadDayActivity {
    /// Thread root date-key.
    pub root_dk: String,
    /// Thread AI summary from *before* today's emails (None if new thread).
    pub thread_ai_before: Option<String>,
    /// Today's email AI summaries and source excerpts, in chronological order.
    pub email_summaries: Vec<DayEmailEvidence>,
    /// Authoritative status for this thread's topic, extracted from
    /// today's "What's cooking in git.git" email if one is present.
    /// `None` when Junio has not published a verdict for this thread,
    /// or when today has no "What's cooking" email at all.
    pub wc_status: Option<WcTopic>,
}

pub struct DayEmailEvidence {
    pub dk: String,
    pub ai: String,
    pub from: Option<String>,
    pub source_excerpt: Option<String>,
    pub source_is_short: bool,
}

/// Output from daily digest generation.
pub struct DayDigestOutput {
    pub human: String,
    pub ai: String,
}

/// Build the daily digest input for a given day.
///
/// `before_commit` is the ref/sha whose `.thread.ai.md` files represent
/// the accumulated thread state *before* today's emails.  For each thread
/// active today, we read the "before" state from that commit.
///
/// `git_ref` is the ref at which today's newly imported `.md` blobs
/// are visible; it is used to load the "What's cooking in git.git"
/// email (if any) and each thread root's Message-ID for
/// reconciliation.
pub fn build_day_digest_input(
    summaries: &[(String, String, String)], // (dk, root_dk, ai_summary)
    before_commit: &str,
    git_ref: &str,
    cat: &mut impl BlobRead,
) -> (Vec<ThreadDayActivity>, usize) {
    use std::collections::BTreeMap;

    let mut by_thread: BTreeMap<String, Vec<DayEmailEvidence>> = BTreeMap::new();
    for (dk, root_dk, ai) in summaries {
        let (from, source_excerpt, source_is_short) = email_evidence(dk, git_ref, cat);
        by_thread
            .entry(root_dk.clone())
            .or_default()
            .push(DayEmailEvidence {
                dk: dk.clone(),
                ai: ai.clone(),
                from,
                source_excerpt,
                source_is_short,
            });
    }

    let email_count = summaries.len();
    let wc_map = load_whats_cooking_map(summaries, git_ref, cat);

    let threads: Vec<ThreadDayActivity> = by_thread
        .into_iter()
        .map(|(root_dk, emails)| {
            let spec = format!("{before_commit}:{root_dk}.thread.ai.md");
            let thread_ai_before = cat.get_str(&spec);
            let wc_status = lookup_wc_status(&root_dk, git_ref, cat, &wc_map);
            ThreadDayActivity {
                root_dk,
                thread_ai_before,
                email_summaries: emails,
                wc_status,
            }
        })
        .collect();

    (threads, email_count)
}

/// Load `<git_ref>:<dk>.md` and return the display name from its
/// `From:` header, or `None` if the blob is missing or the header
/// is empty.
fn email_evidence(
    dk: &str,
    git_ref: &str,
    cat: &mut impl BlobRead,
) -> (Option<String>, Option<String>, bool) {
    let spec = format!("{git_ref}:{dk}.md");
    let Some(md) = cat.get_str(&spec) else {
        return (None, None, false);
    };
    let parsed = rag_parse::parse_email(&md);
    let author = if parsed.author.is_empty() {
        None
    } else {
        Some(parsed.author)
    };
    let authored = authored_email_text(&parsed.body);
    let source_is_short = authored.chars().count() <= 300;
    let excerpt = bounded_excerpt(&authored, 500);
    (
        author,
        Some(format!("Subject: {}\n\n{excerpt}", parsed.subject)),
        source_is_short,
    )
}

fn authored_email_text(body: &str) -> String {
    body.lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn bounded_excerpt(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }

    let half = max_chars / 2;
    let start: String = text.chars().take(half).collect();
    let end: String = text
        .chars()
        .rev()
        .take(max_chars - half)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}\n\n[... source email truncated ...]\n\n{end}")
}

/// Scan today's summaries for a "What's cooking in git.git" email
/// and, if found, parse it into a Message-ID -> topic map.  Returns
/// an empty map when no such email is present.
fn load_whats_cooking_map(
    summaries: &[(String, String, String)],
    git_ref: &str,
    cat: &mut impl BlobRead,
) -> HashMap<String, WcTopic> {
    for (dk, _root, _ai) in summaries {
        let spec = format!("{git_ref}:{dk}.md");
        let Some(md) = cat.get_str(&spec) else {
            continue;
        };
        let parsed = rag_parse::parse_email(&md);
        if parsed.subject.starts_with("What's cooking in git.git") {
            return wc_parse::parse_whats_cooking(&parsed.body);
        }
    }
    HashMap::new()
}

/// Look up a thread root's Message-ID in the parsed "What's cooking"
/// map, returning Junio's authoritative status entry when the thread
/// corresponds to a topic he has weighed in on.
fn lookup_wc_status(
    root_dk: &str,
    git_ref: &str,
    cat: &mut impl BlobRead,
    wc_map: &HashMap<String, WcTopic>,
) -> Option<WcTopic> {
    if wc_map.is_empty() {
        return None;
    }
    let spec = format!("{git_ref}:{root_dk}.md");
    let md = cat.get_str(&spec)?;
    let parsed = rag_parse::parse_email(&md);
    wc_map.get(&parsed.message_id).cloned()
}

/// Generate a daily digest from thread deltas.
///
/// For each thread active today, the AI receives the "before" thread
/// summary and today's individual email summaries, letting it compute
/// the delta.
pub async fn generate_daily_digest(
    day: &str,
    threads: &[ThreadDayActivity],
    email_count: usize,
    backend: &Backend,
) -> Result<DayDigestOutput> {
    let thread_count = threads.len();
    let delta_input = build_daily_digest_user_msg(day, threads, email_count);
    let short_delta_input = build_short_daily_delta_user_msg(day, threads);
    let regular_count = threads
        .iter()
        .flat_map(|thread| &thread.email_summaries)
        .filter(|email| !email.source_is_short)
        .count();
    let short_count = email_count - regular_count;

    eprintln!(
        "[digestive] extracting daily deltas for {day} \
         ({email_count} emails, {thread_count} threads) ...",
    );

    let regular_deltas = if regular_count == 0 {
        String::new()
    } else {
        backend
            .chat_with_options(DAILY_DELTA_AGENT, &delta_input, Some(0.0))
            .await
            .context("daily delta extraction failed")?
    };
    let short_deltas = if short_count == 0 {
        String::new()
    } else {
        backend
            .chat_with_options(DAILY_SHORT_DELTA_AGENT, &short_delta_input, Some(0.0))
            .await
            .context("short daily delta extraction failed")?
    };
    let deltas = [regular_deltas.trim(), short_deltas.trim()]
        .into_iter()
        .filter(|delta| !delta.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let context = build_daily_digest_context(threads, &deltas);

    let user_msg = format!(
        "Date: {day}\nTotal emails today: {email_count}\n\
         Active threads before filtering: {thread_count}\n\n\
         THREAD CONTEXT -- ORIENTATION ONLY; NOT TODAY'S NEWS:\n\n\
         {context}\n\n\
         VERIFIED THREAD DELTAS:\n\n{deltas}",
    );

    eprintln!("[digestive] generating daily digest for {day} ...");

    let human = backend
        .chat_with_options(
            DAILY_DIGEST_AGENT,
            &format!("Mode: human\n\n{user_msg}"),
            Some(0.0),
        )
        .await
        .context("daily digest (human) failed")?;

    let ai = backend
        .chat_with_options(
            DAILY_DIGEST_AGENT,
            &format!("Mode: ai\n\n{user_msg}"),
            Some(0.0),
        )
        .await
        .context("daily digest (AI) failed")?;

    Ok(DayDigestOutput {
        human: strip_digest_links(&summarize::normalize_headings(&human)),
        ai: strip_digest_links(&ai),
    })
}

fn build_daily_digest_context(threads: &[ThreadDayActivity], deltas: &str) -> String {
    let mut context = String::new();

    for thread in threads {
        let root_marker = format!("Thread root: {}", thread.root_dk);
        if !deltas.lines().any(|line| line.trim() == root_marker) {
            continue;
        }

        context.push_str("---\n");
        context.push_str(&root_marker);
        context.push('\n');

        if let Some(before) = &thread.thread_ai_before {
            context.push_str("Context accumulated before today:\n\n");
            context.push_str(before);
            context.push_str("\n\n");
        }

        context.push_str(
            "Today's candidate briefs (topic orientation only; not authoritative \
             for decisions, attribution, review findings, or status):\n\n",
        );
        for email in &thread.email_summaries {
            match &email.from {
                Some(from) => context.push_str(&format!("[{} by {from}]\n", email.dk)),
                None => context.push_str(&format!("[{}]\n", email.dk)),
            }
            context.push_str(&email.ai);
            context.push_str("\n\n");
        }
    }

    context
}

fn strip_digest_links(md: &str) -> String {
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    let re = LINK_RE.get_or_init(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());
    re.replace_all(md, "$1").into_owned()
}

/// Assemble the mode-agnostic user message for `generate_daily_digest`.
///
/// Extracted so that the "Authoritative status" injection can be
/// exercised in a unit test without invoking the backend.
pub(crate) fn build_daily_digest_user_msg(
    day: &str,
    threads: &[ThreadDayActivity],
    email_count: usize,
) -> String {
    let thread_count = threads.len();

    // Compute the weekday name so the LLM does not have to guess it.
    let weekday = crate::date_util::parse_day(day)
        .map(|d| format!("{}", d.weekday()))
        .unwrap_or_default();

    let mut user_msg = format!(
        "Date: {day} ({weekday})\nTotal emails today: {email_count}\n\
         Active threads: {thread_count}\n\n\
         MANDATORY DIFFERENTIAL TASK:\n\
         Each existing thread contains an EXCLUSION BASELINE followed by \
         TODAY'S CANDIDATE BRIEFS and their SOURCE EMAIL evidence. Report only \
         facts or changes introduced today that are absent from the baseline \
         and directly supported by the source email. A candidate brief may \
         repeat, strengthen, or embellish old facts; that does not make them \
         new. Quoted text is context, not today's author's contribution. This \
         is not a request for a current-state recap.\n\n",
    );

    for activity in threads {
        if !activity
            .email_summaries
            .iter()
            .any(|email| !email.source_is_short)
        {
            continue;
        }
        user_msg.push_str("---\n\n");
        user_msg.push_str(&format!("Thread root: {}\n\n", activity.root_dk));

        if let Some(ref wc) = activity.wc_status {
            user_msg.push_str(&format!(
                "Authoritative status (from today's \"What's cooking\"):\n\
                 \x20\x20section: [{}]\n\
                 \x20\x20topic:   {}\n\
                 \x20\x20status:  {}\n\n",
                wc.section, wc.topic, wc.status_line,
            ));
        }

        if let Some(ref before) = activity.thread_ai_before {
            user_msg.push_str(
                "EXCLUSION BASELINE -- FACTS KNOWN BEFORE TODAY; \
                 DO NOT REPORT THEM AS TODAY'S NEWS:\n\n",
            );
            user_msg.push_str(before);
            user_msg.push_str("\n\n");
        } else {
            user_msg.push_str("NO EXCLUSION BASELINE -- this thread is new today.\n\n");
        }

        user_msg.push_str(
            "TODAY'S CANDIDATE BRIEFS -- KEEP ONLY INFORMATION \
             NOT ALREADY IN THE EXCLUSION BASELINE:\n\n",
        );
        for email in &activity.email_summaries {
            if email.source_is_short {
                continue;
            }
            match &email.from {
                Some(f) => user_msg.push_str(&format!("[{} by {f}]\n", email.dk)),
                None => user_msg.push_str(&format!("[{}]\n", email.dk)),
            }
            user_msg.push_str(&email.ai);
            user_msg.push_str("\n\n");
            if let Some(source) = &email.source_excerpt {
                user_msg.push_str("SOURCE EMAIL -- AUTHOR'S UNQUOTED TEXT; GROUND TRUTH:\n\n");
                user_msg.push_str(source);
                user_msg.push_str("\n\n");
            }
        }
    }

    user_msg.push_str(
        "---\n\n\
         FINAL MANDATORY CHECK BEFORE WRITING:\n\
         For every claimed development, identify the specific information \
         in TODAY'S CANDIDATE BRIEFS that was absent from that thread's \
         EXCLUSION BASELINE, then verify it against the author's unquoted \
         SOURCE EMAIL text. Remove the claim if it is unsupported by that \
         text or only repeats quoted context. In particular, never announce \
         an integration state already named in the baseline, even when \
         today's briefs repeat it.\n",
    );

    user_msg
}

fn build_short_daily_delta_user_msg(day: &str, threads: &[ThreadDayActivity]) -> String {
    let mut user_msg = format!(
        "Date: {day}\n\n\
         The following source-only records contain short replies written \
         today. Quoted text, AI briefs, and prior thread summaries are \
         deliberately absent.\n\n",
    );

    for activity in threads {
        for email in &activity.email_summaries {
            if !email.source_is_short {
                continue;
            }
            user_msg.push_str("---\n");
            user_msg.push_str(&format!("Thread root: {}\n", activity.root_dk));
            match &email.from {
                Some(f) => user_msg.push_str(&format!("[{} by {f}]\n", email.dk)),
                None => user_msg.push_str(&format!("[{}]\n", email.dk)),
            }
            if let Some(source) = &email.source_excerpt {
                user_msg.push_str(source);
                user_msg.push_str("\n\n");
            }
        }
    }

    user_msg
}

/// Rebuild one thread's `.thread.ai.md` and `.thread.human.md` from
/// scratch by walking the thread's emails in chronological order and
/// feeding each per-email `.ai.md` back through the thread agent.
///
/// Written for retroactive fixes to threads whose iterative summaries
/// drifted (e.g. the `tc/replay-linearize` thread whose 2026-06-26
/// iteration hallucinated a future merge date that then propagated
/// into every subsequent update).  The rebuilt summaries start from
/// an empty prior state, so any confabulation baked into the
/// existing summary is discarded.
///
/// Writes a single fast-import commit `digestive: resummarize thread
/// <root_dk>` containing the two rebuilt files.
pub async fn resummarize_thread(
    repo_path: &str,
    git_ref: &str,
    root_dk: &str,
    backend: &Backend,
) -> Result<()> {
    let mut cat = CatFile::new(repo_path).context("open target repo")?;
    let (found_root, tree) = thread_file::load_from_repo(&mut cat, git_ref, root_dk)
        .ok_or_else(|| anyhow::anyhow!("no .thread.md found for {root_dk}"))?;
    if found_root != root_dk {
        anyhow::bail!("date-key {root_dk} is not a thread root; its root is {found_root}",);
    }

    let mut dks: Vec<String> = tree.date_keys().map(str::to_string).collect();
    dks.sort();
    if dks.is_empty() {
        anyhow::bail!("thread {root_dk} is empty");
    }

    let system = summarize::thread_system_prompt();
    let mut prev_ai: Option<String> = None;
    let mut prev_human: Option<String> = None;
    for dk in &dks {
        let ai_spec = format!("{git_ref}:{dk}.ai.md");
        let Some(email_ai) = cat.get_str(&ai_spec) else {
            eprintln!("[resummarize] skip {dk} (no .ai.md)");
            continue;
        };
        let from = tree.node_of(dk).map(|n| n.from.clone());
        eprintln!(
            "[resummarize] {dk} by {}",
            from.as_deref().unwrap_or("(unknown)"),
        );
        let human = backend
            .chat_with_options(
                &system,
                &summarize::thread_user_message(
                    prev_human.as_deref(),
                    &email_ai,
                    from.as_deref(),
                    "human",
                ),
                Some(0.0),
            )
            .await
            .with_context(|| format!("thread human summary failed at {dk}"))?;
        let ai_out = backend
            .chat_with_options(
                &system,
                &summarize::thread_user_message(
                    prev_ai.as_deref(),
                    &email_ai,
                    from.as_deref(),
                    "ai",
                ),
                Some(0.0),
            )
            .await
            .with_context(|| format!("thread AI summary failed at {dk}"))?;
        prev_human = Some(summarize::normalize_headings(&human));
        prev_ai = Some(ai_out);
    }

    let final_ai = prev_ai.ok_or_else(|| anyhow::anyhow!("no per-email .ai.md found in thread"))?;
    let final_human = prev_human.expect("populated alongside prev_ai");

    let mut fi = FastImport::new(repo_path, git_ref)?;
    if let Some(oid) = resolve_ref(repo_path, git_ref) {
        fi.set_parent(oid);
    }
    let mut msg = format!("digestive: resummarize thread {root_dk}");
    if let Some(sc) = source_commit_from_ref(repo_path, git_ref) {
        msg.push_str(&format!("\n\nSource-Commit: {sc}"));
    }
    let ai_path = format!("{root_dk}.thread.ai.md");
    let human_path = format!("{root_dk}.thread.human.md");
    fi.commit(
        &msg,
        &[
            (ai_path.as_str(), final_ai.as_str()),
            (human_path.as_str(), final_human.as_str()),
        ],
    )?;
    fi.checkpoint()?;
    fi.finish()?;
    Ok(())
}

/// Regenerate every per-email `.ai.md` and `.human.md` file within
/// one thread by walking its emails in chronological order and
/// invoking `summarize::summarize_email` on each with fresh context.
///
/// Written for retroactive fixes to threads whose per-email briefs
/// themselves contain fabricated claims (typically a per-email
/// summarizer promoting a `seen` topic to "merged").  A subsequent
/// `resummarize_thread` on the same root will then rebuild the
/// thread AI/human summaries from clean inputs, and any downstream
/// `redo_daily` will pick up clean briefs.
///
/// Emits a single fast-import commit `digestive: resummarize
/// per-email briefs in thread <root_dk>` containing all rebuilt
/// files, plus updated `.thread.ai.md` / `.thread.human.md`
/// reflecting the final iteration state.
pub async fn resummarize_email(
    repo_path: &str,
    git_ref: &str,
    root_dk: &str,
    backend: &Backend,
) -> Result<()> {
    let mut cat = CatFile::new(repo_path).context("open target repo")?;
    let (found_root, tree) = thread_file::load_from_repo(&mut cat, git_ref, root_dk)
        .ok_or_else(|| anyhow::anyhow!("no .thread.md found for {root_dk}"))?;
    if found_root != root_dk {
        anyhow::bail!("date-key {root_dk} is not a thread root; its root is {found_root}",);
    }

    let mut dks: Vec<String> = tree.date_keys().map(str::to_string).collect();
    dks.sort();
    if dks.is_empty() {
        anyhow::bail!("thread {root_dk} is empty");
    }

    let mut prev_thread_ai: Option<String> = None;
    let mut prev_thread_human: Option<String> = None;
    // (path, content) pairs to write in the final commit.
    let mut files: Vec<(String, String)> = Vec::new();
    for dk in &dks {
        let md_spec = format!("{git_ref}:{dk}.md");
        let Some(email_md) = cat.get_str(&md_spec) else {
            eprintln!("[resummarize-email] skip {dk} (no .md)");
            continue;
        };

        let parent_dk = tree.parent_of(dk).map(str::to_string);
        let from = tree.node_of(dk).map(|n| n.from.clone());
        let parent_from = parent_dk
            .as_deref()
            .and_then(|p| tree.node_of(p).map(|n| n.from.clone()));

        // Parent's per-email brief comes from what we have already
        // regenerated (freshest) if present in `files`, otherwise
        // fall back to the on-disk (possibly poisoned) version.
        let parent_ai_summary = parent_dk.as_deref().and_then(|p| {
            let want = format!("{p}.ai.md");
            files.iter().find_map(|(path, content)| {
                if path == &want {
                    Some(content.clone())
                } else {
                    None
                }
            })
        });

        eprintln!(
            "[resummarize-email] {dk} by {}",
            from.as_deref().unwrap_or("(unknown)"),
        );

        let ctx = summarize::EmailContext {
            email_md,
            thread_ai_summary: prev_thread_ai.clone(),
            parent_ai_summary,
            from,
            parent_from,
        };
        let out = summarize::summarize_email(&ctx, backend)
            .await
            .with_context(|| format!("resummarize {dk}"))?;

        files.push((format!("{dk}.human.md"), out.human_summary));
        files.push((format!("{dk}.ai.md"), out.ai_summary));
        prev_thread_ai = Some(out.thread_ai_summary.clone());
        prev_thread_human = Some(out.thread_human_summary.clone());
    }

    if files.is_empty() {
        anyhow::bail!("no per-email .md files found in thread");
    }
    // Also refresh the thread summaries to the final iteration state.
    if let (Some(ai), Some(human)) = (prev_thread_ai, prev_thread_human) {
        files.push((format!("{root_dk}.thread.ai.md"), ai));
        files.push((format!("{root_dk}.thread.human.md"), human));
    }

    let mut fi = FastImport::new(repo_path, git_ref)?;
    if let Some(oid) = resolve_ref(repo_path, git_ref) {
        fi.set_parent(oid);
    }
    let mut msg = format!("digestive: resummarize per-email briefs in thread {root_dk}");
    if let Some(sc) = source_commit_from_ref(repo_path, git_ref) {
        msg.push_str(&format!("\n\nSource-Commit: {sc}"));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    fi.commit(&msg, &refs)?;
    fi.checkpoint()?;
    fi.finish()?;
    Ok(())
}

/// Regenerate the daily digest files (`digest.human.md`, `digest.ai.md`)
/// for `day` using the current tip's per-email `.ai.md` files and the
/// previous day's digest tree as the "before" state.
///
/// Written for retroactive fixes to daily digests that consumed a
/// poisoned thread summary: after `resummarize_thread` has cleaned
/// up the offending thread, this rebuilds the digests that were
/// generated on top of the poisoned state.
///
/// Writes a single fast-import commit `digestive: redo daily digest
/// for <day>` containing both files.
pub async fn redo_daily(
    repo_path: &str,
    git_ref: &str,
    day: &str,
    backend: &Backend,
) -> Result<()> {
    let mut cat = CatFile::new(repo_path).context("open target repo")?;

    let head_oid =
        resolve_ref(repo_path, git_ref).ok_or_else(|| anyhow::anyhow!("bad ref: {git_ref}"))?;
    let ls = git_util::git(repo_path, &["ls-tree", "-r", "--name-only", &head_oid, day])
        .context("ls-tree for day")?;
    let mut summaries: Vec<(String, String, String)> = Vec::new();
    for path in ls.lines() {
        if !path.ends_with(".ai.md") {
            continue;
        }
        if path.contains(".thread.") || path.ends_with("/digest.ai.md") {
            continue;
        }
        let dk = path.trim_end_matches(".ai.md").to_string();
        let spec = format!("{git_ref}:{dk}.ai.md");
        let Some(ai) = cat.get_str(&spec) else {
            continue;
        };
        let root_dk = thread_file::load_from_repo(&mut cat, git_ref, &dk)
            .map(|(r, _)| r)
            .unwrap_or_else(|| dk.clone());
        summaries.push((dk, root_dk, ai));
    }
    summaries.sort_by(|a, b| a.0.cmp(&b.0));
    if summaries.is_empty() {
        anyhow::bail!("no per-email .ai.md files found under {day}");
    }

    let prev = add_days(day, -1).ok_or_else(|| anyhow::anyhow!("bad day: {day}"))?;
    let before = find_digest_commit(repo_path, git_ref, &prev).unwrap_or_else(|| head_oid.clone());

    let (threads, email_count) = build_day_digest_input(&summaries, &before, git_ref, &mut cat);
    let digest = generate_daily_digest(day, &threads, email_count, backend).await?;

    let mut fi = FastImport::new(repo_path, git_ref)?;
    if let Some(oid) = resolve_ref(repo_path, git_ref) {
        fi.set_parent(oid);
    }
    let mut msg = format!("digestive: redo daily digest for {day}");
    if let Some(sc) = source_commit_from_ref(repo_path, git_ref) {
        msg.push_str(&format!("\n\nSource-Commit: {sc}"));
    }
    let human_path = format!("{day}/digest.human.md");
    let ai_path = format!("{day}/digest.ai.md");
    fi.commit(
        &msg,
        &[
            (human_path.as_str(), digest.human.as_str()),
            (ai_path.as_str(), digest.ai.as_str()),
        ],
    )?;
    fi.checkpoint()?;
    fi.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cat_file::MockBlobs;

    #[test]
    fn test_load_context_missing_email() {
        let mut blobs = MockBlobs(Default::default());
        let email = EmailToSummarize {
            dk: "2025/01/06/10-00-00".into(),
            root_dk: "2025/01/06/10-00-00".into(),
            parent_dk: None,
        };
        let ctx = load_email_context(&email, &mut blobs, "main");
        assert!(ctx.is_none(), "missing email should return None");
    }

    #[tokio::test]
    async fn summarize_one_soft_skips_on_no_choices() {
        // Simulate the failure mode observed against the Azure
        // OpenAI gateway for oversized prompts: the backend returns
        // HTTP 200 with `choices: []`, which `summarize_email`
        // surfaces as a `NoChoicesError`.  `summarize_one` must
        // treat this as a soft skip (Ok(None)) rather than
        // propagating an error that would abort the pipeline.
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/10-00-00.md".into(), "email body".into());
        let email = EmailToSummarize {
            dk: "2025/01/06/10-00-00".into(),
            root_dk: "2025/01/06/10-00-00".into(),
            parent_dk: None,
        };
        let backend = Backend::MockNoChoices;
        let result = summarize_one(&email, &mut blobs, &backend, "main", None)
            .await
            .expect("no-choices must be a soft skip, not a hard error");
        assert!(result.is_none(), "expected Ok(None) on no-choices skip");
    }

    #[tokio::test]
    async fn summarize_one_generates_stub_on_content_filter() {
        // Azure's Responsible AI content filter rejecting an
        // entirely innocuous technical email (observed in practice
        // against `[PATCH 2/9] setup: stop applying repository
        // format twice`, where the gateway returned HTTP 400 with
        // `finish_reason: content_filter`).  `summarize_one` must
        // generate a mechanical stub from the email Markdown so the
        // email still becomes visible in the per-email .summary.md
        // and .ai.md (and via .ai.md in downstream digests), but
        // mark the result as `SummaryKind::Stub` so flush_batch
        // skips writing thread.*.md.
        let mut blobs = MockBlobs(Default::default());
        let md = "# [PATCH] test\n\n| Header | Value |\n|---|---|\n| **From** | Tester |\n\n---\n\n**Thread**: [t](t.md)\n\nCover letter paragraph.\n";
        blobs
            .0
            .insert("main:2025/06/10/14-57-08.md".into(), md.into());
        let email = EmailToSummarize {
            dk: "2025/06/10/14-57-08".into(),
            root_dk: "2025/06/10/14-57-08".into(),
            parent_dk: None,
        };
        let backend = Backend::MockContentFilter;
        let result = summarize_one(&email, &mut blobs, &backend, "main", None)
            .await
            .expect("content-filter must produce a stub, not a hard error");
        let sf = result.expect("expected stub SummaryFiles, got None");
        assert_eq!(sf.kind, SummaryKind::Stub, "must be marked as a stub");
        assert!(
            sf.human.starts_with("*[Mechanical stub:"),
            "human stub missing marker: {}",
            sf.human
        );
        assert!(sf.ai.contains("[PATCH] test"), "ai stub missing subject");
        assert!(
            sf.thread_human.starts_with("<!-- stub:"),
            "thread_human must be placeholder marker for stubs: {}",
            sf.thread_human
        );
        assert!(
            sf.thread_ai.starts_with("<!-- stub:"),
            "thread_ai must be placeholder marker for stubs: {}",
            sf.thread_ai
        );
    }

    #[test]
    fn rejection_tracking_path_takes_day_part_of_dk() {
        assert_eq!(
            rejection_tracking_path("2026/06/10/14-57-08").as_deref(),
            Some("content-filter-rejections/2026/06/10.md")
        );
        // Missing date-key parts: no tracking file path.
        assert!(rejection_tracking_path("2026/06").is_none());
        assert!(rejection_tracking_path("").is_none());
        assert!(rejection_tracking_path("//").is_none());
    }

    #[test]
    fn make_rejection_entry_includes_subject_author_and_msgid() {
        let md = "# [PATCH 2/9] setup: stop applying repository format twice\n\n\
                  | Header | Value |\n|---|---|\n\
                  | **From** | Patrick Steinhardt <ps@pks.im> |\n\
                  | **Date** | 2026-06-10T16:57:08+02:00 |\n\
                  | **Message-ID** | [20260610-x@pks.im](https://lore.kernel.org/git/20260610-x@pks.im) |\n\n\
                  ---\n\n**Thread**: [t](t.md)\n\nbody\n";
        let entry = make_rejection_entry("2026/06/10/14-57-08", md).expect("entry");
        assert_eq!(entry.dk, "2026/06/10/14-57-08");
        assert!(
            entry.bullet.starts_with("- **14-57-08**"),
            "{}",
            entry.bullet
        );
        assert!(
            entry
                .bullet
                .contains("[\\[PATCH 2/9\\] setup: stop applying repository format twice]")
                || entry
                    .bullet
                    .contains("[[PATCH 2/9] setup: stop applying repository format twice]"),
            "subject missing: {}",
            entry.bullet
        );
        assert!(entry.bullet.contains("../../../2026/06/10/14-57-08.md"));
        assert!(entry.bullet.contains("Patrick Steinhardt"));
        assert!(
            entry
                .bullet
                .contains("https://lore.kernel.org/git/20260610-x@pks.im"),
            "msgid link missing: {}",
            entry.bullet
        );
        assert!(entry.bullet.ends_with('\n'));
    }

    #[test]
    fn merge_rejection_file_creates_fresh_when_no_existing() {
        let entry = RejectionEntry {
            dk: "2026/06/10/14-57-08".into(),
            bullet: "- **14-57-08** [Subject](../../../2026/06/10/14-57-08.md) by *X*\n"
                .to_string(),
        };
        let out = merge_rejection_file("2026-06-10", None, &[entry]);
        assert!(out.starts_with("# Content-filter rejections for 2026-06-10\n\n"));
        assert!(out.contains("- **14-57-08**"));
        assert!(out.ends_with("by *X*\n"));
    }

    #[test]
    fn merge_rejection_file_appends_to_existing_and_dedupes() {
        let existing = "# Content-filter rejections for 2026-06-10\n\n\
             Emails whose AI summarization was rejected by the backend's content filter on this day.  The per-email `.summary.md` and `.ai.md` for these are mechanical stubs (subject, author, opening cover-letter paragraph) rather than real AI summaries; follow the link to read the full message.\n\n\
             - **14-57-08** [Existing](../../../2026/06/10/14-57-08.md) by *Alice*\n";
        let new = vec![
            // Same dk as existing -- must be deduped.
            RejectionEntry {
                dk: "2026/06/10/14-57-08".into(),
                bullet: "- **14-57-08** [Duplicate](../../../2026/06/10/14-57-08.md) by *Bob*\n"
                    .to_string(),
            },
            // New dk -- must be appended.
            RejectionEntry {
                dk: "2026/06/10/22-00-00".into(),
                bullet: "- **22-00-00** [New](../../../2026/06/10/22-00-00.md) by *Carol*\n"
                    .to_string(),
            },
        ];
        let out = merge_rejection_file("2026-06-10", Some(existing), &new);
        assert!(out.contains("- **14-57-08** [Existing]"));
        assert!(
            !out.contains("by *Bob*"),
            "duplicate must be dropped: {out}"
        );
        assert!(out.contains("- **22-00-00** [New]"));
        // The header must not be duplicated.
        assert_eq!(
            out.matches("# Content-filter rejections for 2026-06-10")
                .count(),
            1,
            "header duplicated: {out}"
        );
    }

    #[test]
    fn merge_rejection_file_with_no_new_entries_preserves_existing() {
        let existing = "# Content-filter rejections for 2026-06-10\n\n\
             Emails whose AI summarization was rejected by the backend's content filter on this day.  The per-email `.summary.md` and `.ai.md` for these are mechanical stubs (subject, author, opening cover-letter paragraph) rather than real AI summaries; follow the link to read the full message.\n\n\
             - **14-57-08** [Existing](../../../2026/06/10/14-57-08.md) by *Alice*\n";
        let out = merge_rejection_file("2026-06-10", Some(existing), &[]);
        assert_eq!(out, existing, "no-op merge must preserve existing content");
    }

    #[test]
    fn test_load_context_no_thread() {
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/10-00-00.md".into(), "email body".into());
        let email = EmailToSummarize {
            dk: "2025/01/06/10-00-00".into(),
            root_dk: "2025/01/06/10-00-00".into(),
            parent_dk: None,
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert_eq!(ctx.email_md, "email body");
        assert!(ctx.thread_ai_summary.is_none());
    }

    fn sf_ai(dk: &str, root_dk: &str) -> SummaryFiles {
        SummaryFiles {
            dk: dk.to_string(),
            root_dk: root_dk.to_string(),
            human: format!("human:{dk}"),
            ai: format!("ai:{dk}"),
            thread_human: format!("thread.human:{root_dk}"),
            thread_ai: format!("thread.ai:{root_dk}"),
            kind: SummaryKind::AiGenerated,
        }
    }

    fn sf_stub(dk: &str, root_dk: &str) -> SummaryFiles {
        SummaryFiles {
            dk: dk.to_string(),
            root_dk: root_dk.to_string(),
            human: format!("human-stub:{dk}"),
            ai: format!("ai-stub:{dk}"),
            thread_human: "<!-- stub -->\n".to_string(),
            thread_ai: "<!-- stub -->\n".to_string(),
            kind: SummaryKind::Stub,
        }
    }

    #[test]
    fn summary_files_to_refs_emits_all_four_for_ai_generated() {
        let batch = [sf_ai("2025/01/06/10-00-00", "2025/01/06/10-00-00")];
        let refs = summary_files_to_refs(&batch);
        let paths: Vec<&str> = refs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "2025/01/06/10-00-00.human.md",
                "2025/01/06/10-00-00.ai.md",
                "2025/01/06/10-00-00.thread.human.md",
                "2025/01/06/10-00-00.thread.ai.md",
            ]
        );
    }

    #[test]
    fn summary_files_to_refs_emits_only_per_email_for_stub() {
        let batch = [sf_stub("2025/06/10/14-57-08", "2025/06/10/14-57-06")];
        let refs = summary_files_to_refs(&batch);
        let paths: Vec<&str> = refs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["2025/06/10/14-57-08.human.md", "2025/06/10/14-57-08.ai.md",],
            "stubs must not write the cumulative thread.*.md files"
        );
    }

    #[test]
    fn summary_files_to_refs_mixed_batch_preserves_order() {
        let batch = [
            sf_ai("2025/06/10/10-00-00", "2025/06/10/10-00-00"),
            sf_stub("2025/06/10/14-57-08", "2025/06/10/14-57-06"),
            sf_ai("2025/06/10/15-00-00", "2025/06/10/10-00-00"),
        ];
        let refs = summary_files_to_refs(&batch);
        let paths: Vec<&str> = refs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                // First (AI): all four files.
                "2025/06/10/10-00-00.human.md",
                "2025/06/10/10-00-00.ai.md",
                "2025/06/10/10-00-00.thread.human.md",
                "2025/06/10/10-00-00.thread.ai.md",
                // Second (Stub): per-email only.
                "2025/06/10/14-57-08.human.md",
                "2025/06/10/14-57-08.ai.md",
                // Third (AI): all four files again; this email's
                // thread root happens to be the same as the first
                // entry's, so the thread.*.md path repeats -- the
                // last write wins, which is the desired behavior.
                "2025/06/10/15-00-00.human.md",
                "2025/06/10/15-00-00.ai.md",
                "2025/06/10/10-00-00.thread.human.md",
                "2025/06/10/10-00-00.thread.ai.md",
            ]
        );
    }

    #[test]
    fn test_load_context_thread_from_repo() {
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/12-00-00.md".into(), "reply body".into());
        blobs.0.insert(
            "main:2025/01/05/09-00-00.thread.ai.md".into(),
            "repo thread".into(),
        );
        let email = EmailToSummarize {
            dk: "2025/01/06/12-00-00".into(),
            root_dk: "2025/01/05/09-00-00".into(),
            parent_dk: None,
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert_eq!(ctx.thread_ai_summary.as_deref(), Some("repo thread"));
    }

    #[test]
    fn test_load_context_cache_takes_precedence() {
        // With CachedReader, "cache takes precedence" means that
        // inserting into the BlobRead impl shadows the underlying
        // git data. We simulate this with MockBlobs by inserting
        // both the "repo" value and the "cache" value at the same
        // key — the last insert wins.
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/12-00-00.md".into(), "reply body".into());
        // The "cached" thread AI summary shadows any repo value:
        blobs.0.insert(
            "main:2025/01/05/09-00-00.thread.ai.md".into(),
            "cached thread".into(),
        );
        let email = EmailToSummarize {
            dk: "2025/01/06/12-00-00".into(),
            root_dk: "2025/01/05/09-00-00".into(),
            parent_dk: None,
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert_eq!(
            ctx.thread_ai_summary.as_deref(),
            Some("cached thread"),
            "in-memory cache should take precedence over repo"
        );
    }

    #[test]
    fn test_load_context_with_parent() {
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/12-00-00.md".into(), "reply body".into());
        blobs.0.insert(
            "main:2025/01/06/10-00-00.ai.md".into(),
            "parent summary".into(),
        );
        let email = EmailToSummarize {
            dk: "2025/01/06/12-00-00".into(),
            root_dk: "2025/01/06/10-00-00".into(),
            parent_dk: Some("2025/01/06/10-00-00".into()),
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert_eq!(ctx.parent_ai_summary.as_deref(), Some("parent summary"));
    }

    #[test]
    fn test_load_context_parent_missing_summary() {
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("main:2025/01/06/12-00-00.md".into(), "reply body".into());
        // parent_dk is set but no .ai.md exists for it
        let email = EmailToSummarize {
            dk: "2025/01/06/12-00-00".into(),
            root_dk: "2025/01/06/10-00-00".into(),
            parent_dk: Some("2025/01/06/10-00-00".into()),
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert!(
            ctx.parent_ai_summary.is_none(),
            "missing parent .ai.md should yield None"
        );
    }

    #[test]
    fn test_build_day_digest_new_thread() {
        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(
            "test:2025/01/06/10-00-00.md".into(),
            "# Subject\n\n| **From** | Example Author <author@example.com> |\n\n---\n\nBody".into(),
        );
        let summaries = vec![(
            "2025/01/06/10-00-00".into(),
            "2025/01/06/10-00-00".into(),
            "email ai".into(),
        )];
        let (threads, count) = build_day_digest_input(&summaries, "before", "test", &mut blobs);
        assert_eq!(count, 1);
        assert_eq!(threads.len(), 1);
        assert!(
            threads[0].thread_ai_before.is_none(),
            "new thread should have no before state"
        );
        assert_eq!(
            threads[0].email_summaries[0].from.as_deref(),
            Some("Example Author <author@example.com>")
        );
        assert!(
            threads[0].email_summaries[0]
                .source_excerpt
                .as_deref()
                .is_some_and(|source| source.contains("Body")),
            "source email should accompany its untrusted summary"
        );
        assert!(
            threads[0].email_summaries[0].source_is_short,
            "short authored text should suppress the candidate brief"
        );
    }

    #[test]
    fn test_authored_email_text_removes_quote() {
        let email = format!(
            "On Thursday, Junio wrote:\n{}\nIt looks ready to me\n\nThanks",
            "> quoted context\n".repeat(200)
        );
        let authored = authored_email_text(&email);

        assert!(authored.starts_with("On Thursday"));
        assert!(!authored.contains("quoted context"));
        assert!(authored.contains("It looks ready to me"));
        assert!(authored.ends_with("Thanks"));
    }

    #[test]
    fn test_build_day_digest_existing_thread() {
        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(
            "before:2025/01/05/09-00-00.thread.ai.md".into(),
            "prior thread summary".into(),
        );
        let summaries = vec![(
            "2025/01/06/10-00-00".into(),
            "2025/01/05/09-00-00".into(),
            "reply ai".into(),
        )];
        let (threads, _) = build_day_digest_input(&summaries, "before", "test", &mut blobs);
        assert_eq!(
            threads[0].thread_ai_before.as_deref(),
            Some("prior thread summary")
        );
    }

    #[test]
    fn test_build_day_digest_ignores_post_digest_thread_update() {
        let mut blobs = MockBlobs(Default::default());
        let summaries = vec![(
            "2025/01/06/12-00-00".into(),
            "2025/01/05/09-00-00".into(),
            "new reply".into(),
        )];
        let (threads, _) = build_day_digest_input(&summaries, "before", "test", &mut blobs);
        assert!(
            threads[0].thread_ai_before.is_none(),
            "should not see thread summary from a commit after the daily digest"
        );
    }

    #[test]
    fn test_build_day_digest_multiple_threads() {
        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(
            "before:2025/01/03/08-00-00.thread.ai.md".into(),
            "old thread state".into(),
        );
        let summaries = vec![
            (
                "2025/01/06/10-00-00".into(),
                "2025/01/03/08-00-00".into(),
                "reply1".into(),
            ),
            (
                "2025/01/06/11-00-00".into(),
                "2025/01/06/11-00-00".into(),
                "new thread".into(),
            ),
            (
                "2025/01/06/12-00-00".into(),
                "2025/01/03/08-00-00".into(),
                "reply2".into(),
            ),
        ];
        let (threads, count) = build_day_digest_input(&summaries, "before", "test", &mut blobs);
        assert_eq!(count, 3);
        assert_eq!(threads.len(), 2);

        let old = threads
            .iter()
            .find(|t| t.root_dk == "2025/01/03/08-00-00")
            .unwrap();
        assert_eq!(old.thread_ai_before.as_deref(), Some("old thread state"));
        assert_eq!(
            old.email_summaries.len(),
            2,
            "two replies in existing thread"
        );

        let new = threads
            .iter()
            .find(|t| t.root_dk == "2025/01/06/11-00-00")
            .unwrap();
        assert!(
            new.thread_ai_before.is_none(),
            "new thread has no prior state"
        );
        assert_eq!(new.email_summaries.len(), 1);
    }

    /// Populates the "What's cooking" map when a matching email is
    /// present in today's summaries, and threads the resulting
    /// status back to the affected thread via the root's Message-ID.
    #[test]
    fn test_build_day_digest_reconciles_with_whats_cooking() {
        const WC_DK: &str = "2026/07/01/23-40-16";
        const ROOT_DK: &str = "2026/06/26/05-48-11";
        const ROOT_MSGID: &str = "20260626-toon-git-replay-drop-merges-v5-0-5e120738b9d0@iotcl.com";

        let wc_md = format!(
            "# What's cooking in git.git (Jul 2026, #01)\n\n\
            | Header | Value |\n|---|---|\n| **From** | Junio |\n\
            \n**Thread**: [t](t.md)\n\n\
            [Cooking]\n\n\
            * tc/replay-linearize (2026-06-25) 3 commits\n\
            \x20- replay: offer an option to linearize the commit topology\n\n\
            \x20git replay learns --linearize option.\n\n\
            \x20Waiting for response(s) to review comment(s).\n\
            \x20source: <{ROOT_MSGID}>\n",
        );
        let root_md = format!(
            "# [PATCH v5 1/3] replay: add helper\n\n\
            | Header | Value |\n|---|---|\n\
            | **From** | Toon |\n\
            | **Date** | 2026-06-26T07:48:11+02:00 |\n\
            | **Message-ID** | [{ROOT_MSGID}](https://lore.kernel.org/git/{ROOT_MSGID}) |\n\
            \n**Thread**: [t](t.md)\n\n\
            Patch body.\n",
        );

        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(format!("test:{WC_DK}.md"), wc_md);
        blobs.0.insert(format!("test:{ROOT_DK}.md"), root_md);

        let summaries = vec![
            (WC_DK.into(), WC_DK.into(), "wc summary".into()),
            (ROOT_DK.into(), ROOT_DK.into(), "linearize summary".into()),
        ];
        let (threads, _) = build_day_digest_input(&summaries, "before", "test", &mut blobs);

        let linearize = threads.iter().find(|t| t.root_dk == ROOT_DK).unwrap();
        let wc = linearize
            .wc_status
            .as_ref()
            .expect("linearize thread must get Junio's authoritative status");
        assert_eq!(wc.section, "Cooking");
        assert_eq!(wc.topic, "tc/replay-linearize");
        assert_eq!(
            wc.status_line,
            "Waiting for response(s) to review comment(s).",
        );

        let wc_thread = threads.iter().find(|t| t.root_dk == WC_DK).unwrap();
        assert!(
            wc_thread.wc_status.is_none(),
            "the What's cooking email's own thread must not be marked with itself as a topic",
        );
    }

    /// The daily-digest user message must surface Junio's status
    /// verbatim so the LLM cannot invent a competing claim.
    #[test]
    fn test_daily_digest_user_msg_includes_authoritative_status() {
        let activity = ThreadDayActivity {
            root_dk: "2026/06/26/05-48-11".into(),
            thread_ai_before: Some("prior summary claiming merged to master".into()),
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/07/01/08-50-41".into(),
                ai: "reply summary".into(),
                from: Some("Toon Claes".into()),
                source_excerpt: Some("The source reply.".into()),
                source_is_short: false,
            }],
            wc_status: Some(WcTopic {
                section: "Cooking".into(),
                topic: "tc/replay-linearize".into(),
                status_line: "Waiting for response(s) to review comment(s).".into(),
            }),
        };
        let msg = build_daily_digest_user_msg("2026/07/01", &[activity], 1);

        assert!(
            msg.contains("Authoritative status"),
            "should announce the authoritative block: {msg}",
        );
        assert!(msg.contains("section: [Cooking]"), "missing section");
        assert!(
            msg.contains("topic:   tc/replay-linearize"),
            "missing topic"
        );
        assert!(
            msg.contains("status:  Waiting for response(s) to review comment(s)."),
            "missing status line verbatim",
        );
        assert!(
            msg.contains("prior summary claiming merged to master"),
            "prior summary must still be visible so the model can note the discrepancy",
        );
        assert!(
            msg.contains("[2026/07/01/08-50-41 by Toon Claes]"),
            "each per-email brief should announce its author via a `by <Name>` header, got:\n{msg}",
        );
        assert!(
            msg.contains("MANDATORY DIFFERENTIAL TASK"),
            "missing differential task framing",
        );
        assert!(
            msg.contains("EXCLUSION BASELINE -- FACTS KNOWN BEFORE TODAY"),
            "prior state should be marked as an exclusion baseline",
        );
        assert!(
            msg.contains("TODAY'S CANDIDATE BRIEFS"),
            "today's summaries should be marked as candidates, not facts",
        );
        assert!(
            msg.contains(
                "SOURCE EMAIL -- AUTHOR'S UNQUOTED TEXT; GROUND TRUTH:\n\n\
                 The source reply.",
            ),
            "source evidence should be separated from the untrusted summary",
        );
        assert!(
            msg.contains("FINAL MANDATORY CHECK BEFORE WRITING"),
            "missing recency-positioned differential check",
        );
    }

    #[test]
    fn test_short_daily_delta_input_excludes_summaries_and_baseline() {
        let activity = ThreadDayActivity {
            root_dk: "2026/05/01/21-35-37".into(),
            thread_ai_before: Some("poisoned prior rationale".into()),
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/08/07/13-09-14".into(),
                ai: "poisoned candidate rationale".into(),
                from: Some("Phillip Wood".into()),
                source_excerpt: Some(
                    "Subject: Re: [PATCH v25 0/7] branch: delete-merged\n\n\
                     It looks ready to me\n\nThanks\n\nPhillip"
                        .into(),
                ),
                source_is_short: true,
            }],
            wc_status: None,
        };

        let regular = build_daily_digest_user_msg("2026/08/07", &[activity], 1);
        assert!(!regular.contains("poisoned prior rationale"));
        assert!(!regular.contains("poisoned candidate rationale"));

        let activity = ThreadDayActivity {
            root_dk: "2026/05/01/21-35-37".into(),
            thread_ai_before: Some("poisoned prior rationale".into()),
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/08/07/13-09-14".into(),
                ai: "poisoned candidate rationale".into(),
                from: Some("Phillip Wood".into()),
                source_excerpt: Some(
                    "Subject: Re: [PATCH v25 0/7] branch: delete-merged\n\n\
                     It looks ready to me\n\nThanks\n\nPhillip"
                        .into(),
                ),
                source_is_short: true,
            }],
            wc_status: None,
        };
        let short = build_short_daily_delta_user_msg("2026/08/07", &[activity]);
        assert!(short.contains("It looks ready to me"));
        assert!(!short.contains("poisoned prior rationale"));
        assert!(!short.contains("poisoned candidate rationale"));
    }

    #[test]
    fn test_daily_digest_context_keeps_topic_background_separate() {
        let activity = ThreadDayActivity {
            root_dk: "2026/08/13/05-47-56".into(),
            thread_ai_before: Some(
                "This refactoring changes object database lookup to avoid \
                 repository-global state."
                    .into(),
            ),
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/08/13/17-35-39".into(),
                ai: "Junio agrees to replace the older topic with the corrected revision.".into(),
                from: Some("Junio C Hamano".into()),
                source_excerpt: Some("Will do.".into()),
                source_is_short: true,
            }],
            wc_status: None,
        };

        let deltas = "---\nThread root: 2026/08/13/05-47-56\nNew today:\n\
                      - [2026/08/13/17-35-39 by Junio C Hamano] Agreed.";
        let context = build_daily_digest_context(&[activity], deltas);
        assert!(context.contains("Thread root: 2026/08/13/05-47-56"));
        assert!(context.contains("Context accumulated before today:"));
        assert!(context.contains("object database lookup"));
        assert!(context.contains("Today's candidate briefs (topic orientation only"));
        assert!(context.contains("[2026/08/13/17-35-39 by Junio C Hamano]"));
    }

    #[test]
    fn test_daily_digest_context_omits_threads_without_deltas() {
        let activity = ThreadDayActivity {
            root_dk: "2026/08/12/00-00-00".into(),
            thread_ai_before: Some("An old integration milestone.".into()),
            email_summaries: Vec::new(),
            wc_status: None,
        };

        let context = build_daily_digest_context(
            &[activity],
            "---\nThread root: 2026/08/13/05-47-56\nNew today:\n- New work.",
        );
        assert!(context.is_empty());
    }

    #[test]
    fn test_strip_digest_links_preserves_text() {
        let digest = "Elijah [reviewed](2026/08/07/03-02-04) it; see \
                      [details](https://example.com/wrong).";
        assert_eq!(
            strip_digest_links(digest),
            "Elijah reviewed it; see details."
        );
    }

    /// Backfill test: a reply to a dormant thread triggers
    /// summarization of the thread root, even though the root
    /// is before the --since cutoff.
    ///
    /// Timeline:
    ///   2024/12/15: Old email (thread root, no .ai.md)
    ///   2025/01/20: Reply (within --since range)
    ///   2025/02/01: Second email (new thread root, for day boundary)
    ///
    /// Expected: backfill generates .ai.md for the 2024/12/15 root
    /// before summarizing the 2025/01/20 reply.
    #[tokio::test]
    async fn test_thread_backfill() {
        use crate::ai_backend::Backend;

        const OLD_ROOT: &str = "2024/12/15/10-00-00";
        const REPLY: &str = "2025/01/20/14-00-00";
        const STANDALONE: &str = "2025/02/01/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            fi.commit_with_symlinks("seed: dormant thread + reply", &[
                (&format!("{OLD_ROOT}.md"),
                 "Subject: Old discussion\nFrom: Oldie\nDate: Sun, 15 Dec 2024\n\nAncient text."),
                (&format!("{OLD_ROOT}.thread.md"),
                 "# Thread\n\n- 2024/12/15/10-00-00 [Old discussion](10-00-00.md) *Oldie*\n"),
                (&format!("{REPLY}.md"),
                 "Subject: Re: Old discussion\nFrom: Newbie\nDate: Mon, 20 Jan 2025\n\nLate reply."),
                (&format!("{STANDALONE}.md"),
                 "Subject: Fresh topic\nFrom: Solo\nDate: Sat, 1 Feb 2025\n\nNew."),
                (&format!("{STANDALONE}.thread.md"),
                 "# Thread\n\n- 2025/02/01/09-00-00 [Fresh topic](09-00-00.md) *Solo*\n"),
            ], &[
                (&format!("{REPLY}.thread.md"),
                 "../../../2024/12/15/10-00-00.thread.md"),
            ], &[]).unwrap();

            fi.finish().unwrap();
        }

        // The old root has no .ai.md. Run with --since 2025/01/01
        // so only the reply and standalone are in range.
        let backend = Backend::Mock { nth_word: 3 };
        let result = run_pipeline(
            repo,
            git_ref,
            Some("2025/01/01"),
            None,
            5,
            Some(&backend),
            false,
        )
        .await
        .unwrap();

        // The old root should have been backfilled + reply + standalone.
        assert_eq!(
            result.total_processed, 3,
            "should summarize old root (backfill) + reply + standalone"
        );

        let mut cat = CatFile::new(repo).unwrap();

        // Old root (before --since) got backfilled.
        assert!(
            cat.get_str(&format!("{git_ref}:{OLD_ROOT}.ai.md"))
                .is_some(),
            "old root should have AI summary via backfill"
        );

        // Reply got summarized.
        assert!(
            cat.get_str(&format!("{git_ref}:{REPLY}.ai.md")).is_some(),
            "reply should have AI summary"
        );

        // Standalone got summarized.
        assert!(
            cat.get_str(&format!("{git_ref}:{STANDALONE}.ai.md"))
                .is_some(),
            "standalone should have AI summary"
        );

        // git fsck --strict.
        drop(cat);
        crate::git_util::tests::git(repo, &["fsck", "--strict"]);
    }

    /// Verify that backfill does not re-summarize emails quadratically.
    ///
    /// When a thread has N unsummarized members, a naive implementation
    /// would re-summarize all prior members for each new reply because
    /// intermediate results are not visible until a checkpoint lands.
    /// The fix caches `.ai.md` in memory so each email is summarized
    /// exactly once.
    ///
    /// Setup: root + 2 replies + 1 next-day email, none with `.ai.md`.
    /// Expected: 4 emails × 1 summarization each = 4.
    #[tokio::test]
    async fn test_no_quadratic_resummarization() {
        use crate::ai_backend::Backend;

        const ROOT: &str = "2025/04/01/10-00-00";
        const REPLY1: &str = "2025/04/01/11-00-00";
        const REPLY2: &str = "2025/04/01/12-00-00";
        const NEXT_DAY: &str = "2025/04/02/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            let thread_md = [
                "# Thread",
                "",
                &format!("- {ROOT} [topic](10-00-00.md) *Alice*"),
                &format!("  - {REPLY1} [Re: topic](11-00-00.md) *Bob*"),
                &format!("  - {REPLY2} [Re: topic](12-00-00.md) *Carol*"),
                "",
            ]
            .join("\n");

            fi.commit_with_symlinks(
                "seed",
                &[
                    (
                        &format!("{ROOT}.md"),
                        "Subject: topic\nFrom: Alice\nDate: Tue, 1 Apr 2025\n\nOriginal.",
                    ),
                    (&format!("{ROOT}.thread.md"), &thread_md),
                    (
                        &format!("{REPLY1}.md"),
                        "Subject: Re: topic\nFrom: Bob\nDate: Tue, 1 Apr 2025\n\nReply one.",
                    ),
                    (
                        &format!("{REPLY2}.md"),
                        "Subject: Re: topic\nFrom: Carol\nDate: Tue, 1 Apr 2025\n\nReply two.",
                    ),
                    (
                        &format!("{NEXT_DAY}.md"),
                        "Subject: other\nFrom: Dave\nDate: Wed, 2 Apr 2025\n\nNew.",
                    ),
                    (
                        &format!("{NEXT_DAY}.thread.md"),
                        "# Thread\n\n- 2025/04/02/09-00-00 [other](09-00-00.md) *Dave*\n",
                    ),
                ],
                &[
                    (&format!("{REPLY1}.thread.md"), "10-00-00.thread.md"),
                    (&format!("{REPLY2}.thread.md"), "10-00-00.thread.md"),
                ],
                &[],
            )
            .unwrap();

            fi.finish().unwrap();
        }

        let backend = Backend::Mock { nth_word: 3 };
        let result = run_pipeline(repo, git_ref, None, None, 20, Some(&backend), false)
            .await
            .unwrap();

        // 4 unique emails need summarization: root, reply1, reply2,
        // standalone.  Each should be summarized exactly once.
        assert_eq!(
            result.total_processed, 4,
            "each email should be summarized exactly once; \
             got {} (quadratic re-summarization bug)",
            result.total_processed
        );
    }

    /// Verify that backfill_thread does NOT process thread members
    /// beyond the triggering email's datekey.
    ///
    /// Setup: a thread root on day 1 with replies on day 1 and day 2.
    /// None have AI summaries.  The main loop processes day 1 first.
    /// The day-1 reply triggers backfill, which must NOT summarize
    /// the day-2 reply (it belongs to day 2's digest, not day 1's).
    #[tokio::test]
    async fn test_backfill_bounded_by_trigger_dk() {
        use crate::ai_backend::Backend;

        let root = "2025/03/10/10-00-00";
        let day1_reply = "2025/03/10/14-00-00";
        let day2_reply = "2025/03/11/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            let thread_md = [
                "# Thread",
                "",
                &format!("- {root} [topic](10-00-00.md) *Alice*"),
                &format!("  - {day1_reply} [Re: topic](14-00-00.md) *Bob*"),
                &format!("  - {day2_reply} [Re: topic](09-00-00.md) *Carol*"),
            ]
            .join("\n");

            fi.commit(
                "seed",
                &[
                    (
                        &format!("{root}.md"),
                        "Subject: topic\nFrom: Alice\n\ntopic",
                    ),
                    (&format!("{root}.thread.md"), &thread_md),
                    (
                        &format!("{day1_reply}.md"),
                        "Subject: Re: topic\nFrom: Bob\n\nreply1",
                    ),
                    (&format!("{day1_reply}.thread.md"), &thread_md),
                    (
                        &format!("{day2_reply}.md"),
                        "Subject: Re: topic\nFrom: Carol\n\nreply2",
                    ),
                    (&format!("{day2_reply}.thread.md"), &thread_md),
                    // Add a day-3 email to trigger the day-2 boundary
                    // (finalize_day only fires at the next day boundary).
                    (
                        "2025/03/12/10-00-00.md",
                        "Subject: other\nFrom: Dave\n\nother",
                    ),
                    (
                        "2025/03/12/10-00-00.thread.md",
                        "# Thread\n\n- 2025/03/12/10-00-00 [other](10-00-00.md) *Dave*\n",
                    ),
                ],
            )
            .unwrap();

            fi.finish().unwrap();
        }

        let backend = Backend::Mock { nth_word: 1 };
        let result = run_pipeline(repo, git_ref, None, None, 20, Some(&backend), false)
            .await
            .unwrap();

        // All 4 emails should be summarized.
        assert_eq!(
            result.total_processed, 4,
            "expected 4 summaries; got {}",
            result.total_processed
        );

        let mut cat = CatFile::new(repo).unwrap();

        // Day 1 (Mar 10) daily digest should include root + day1_reply.
        let day1_digest = cat
            .get_str(&format!("{git_ref}:2025/03/10/digest.human.md"))
            .expect("day 1 digest should exist");
        assert!(
            day1_digest.contains("2025/03/10/10-00-00"),
            "day 1 digest should include the root email"
        );
        assert!(
            day1_digest.contains("2025/03/10/14-00-00"),
            "day 1 digest should include the day-1 reply"
        );
        // The day-2 reply must NOT leak into day 1's digest.
        assert!(
            !day1_digest.contains("2025/03/11"),
            "day 1 digest must NOT contain day-2 content"
        );

        // Day 2 (Mar 11) daily digest should include day2_reply.
        let day2_digest = cat
            .get_str(&format!("{git_ref}:2025/03/11/digest.human.md"))
            .expect("day 2 digest should exist");
        assert!(
            day2_digest.contains("2025/03/11/09-00-00"),
            "day 2 digest should include the day-2 reply"
        );

        drop(cat);
        crate::git_util::tests::git(repo, &["fsck", "--strict"]);
    }

    /// Test cross-boundary digest rollup with --since at a mid-week day.
    ///
    /// Calendar: 2025/01/01 is Wednesday.
    /// ISO week: Mon Dec 30 – Sun Jan 05.
    ///
    /// Setup:
    ///   Dec 30 (Mon): email + .ai.md + daily digest  (pre-since)
    ///   Dec 31 (Tue): email + .ai.md, NO daily digest (pre-since, gap)
    ///   Jan 01 (Wed): email + .ai.md + daily digest  (in range)
    ///   Jan 02 (Thu): email + .ai.md, NO daily digest (in range, missing)
    ///   Jan 06 (Mon): email .md only                  (new week)
    ///   Feb 01 (Sat): email .md only                  (triggers month boundary)
    ///
    /// No weekly or monthly digests pre-exist.
    ///
    /// Verifications:
    ///   - Dec 30, Dec 31 daily digests NOT generated (pre-since skip)
    ///   - Dec 31 stays without daily digest (gap preserved)
    ///   - Jan 02 daily digest IS generated
    ///   - Weekly Jan 05 picks up dailies for Dec 30, Jan 01, Jan 02
    ///     (NOT Dec 31: it has no daily digest)
    ///   - Jan 06 email is summarized, daily digest generated
    ///   - Weekly Jan 12 rolls up Jan 06's daily
    ///   - Monthly Jan rolls up both weekly digests (Jan 05 + Jan 12)
    #[tokio::test]
    async fn test_cross_boundary_digest_rollup() {
        use crate::ai_backend::Backend;

        let dec30 = "2024/12/30/10-00-00";
        let dec31 = "2024/12/31/10-00-00";
        let jan01 = "2025/01/01/10-00-00";
        let jan02 = "2025/01/02/10-00-00";
        let jan06 = "2025/01/06/10-00-00";
        let feb01 = "2025/02/01/10-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            // Each email is a standalone thread root.
            fi.commit(
                "seed: emails and pre-existing summaries",
                &[
                    // Dec 30: email + .ai.md + daily digest (pre-since)
                    (
                        &format!("{dec30}.md"),
                        "Subject: Mon\nFrom: A\nDate: Mon, 30 Dec 2024\n\nMon.",
                    ),
                    (&format!("{dec30}.ai.md"), "AI summary for Dec 30"),
                    (&format!("{dec30}.human.md"), "Human summary for Dec 30"),
                    (
                        &format!("{dec30}.thread.md"),
                        &format!("# Thread\n\n- {dec30} [Mon](10-00-00.md) *A*\n"),
                    ),
                    (&format!("{dec30}.thread.ai.md"), "thread ai Dec 30"),
                    (&format!("{dec30}.thread.human.md"), "thread human Dec 30"),
                    ("2024/12/30/digest.human.md", "Daily digest Dec 30 human"),
                    ("2024/12/30/digest.ai.md", "Daily digest Dec 30 ai"),
                    // Dec 31: email + .ai.md but NO daily digest (gap)
                    (
                        &format!("{dec31}.md"),
                        "Subject: Tue\nFrom: B\nDate: Tue, 31 Dec 2024\n\nTue.",
                    ),
                    (&format!("{dec31}.ai.md"), "AI summary for Dec 31"),
                    (&format!("{dec31}.human.md"), "Human summary for Dec 31"),
                    (
                        &format!("{dec31}.thread.md"),
                        &format!("# Thread\n\n- {dec31} [Tue](10-00-00.md) *B*\n"),
                    ),
                    (&format!("{dec31}.thread.ai.md"), "thread ai Dec 31"),
                    (&format!("{dec31}.thread.human.md"), "thread human Dec 31"),
                    // Jan 01: email + .ai.md + daily digest (in range)
                    (
                        &format!("{jan01}.md"),
                        "Subject: Wed\nFrom: C\nDate: Wed, 1 Jan 2025\n\nWed.",
                    ),
                    (&format!("{jan01}.ai.md"), "AI summary for Jan 01"),
                    (&format!("{jan01}.human.md"), "Human summary for Jan 01"),
                    (
                        &format!("{jan01}.thread.md"),
                        &format!("# Thread\n\n- {jan01} [Wed](10-00-00.md) *C*\n"),
                    ),
                    (&format!("{jan01}.thread.ai.md"), "thread ai Jan 01"),
                    (&format!("{jan01}.thread.human.md"), "thread human Jan 01"),
                    ("2025/01/01/digest.human.md", "Daily digest Jan 01 human"),
                    ("2025/01/01/digest.ai.md", "Daily digest Jan 01 ai"),
                    // Jan 02: email + .ai.md but NO daily digest (missing)
                    (
                        &format!("{jan02}.md"),
                        "Subject: Thu\nFrom: D\nDate: Thu, 2 Jan 2025\n\nThu.",
                    ),
                    (&format!("{jan02}.ai.md"), "AI summary for Jan 02"),
                    (&format!("{jan02}.human.md"), "Human summary for Jan 02"),
                    (
                        &format!("{jan02}.thread.md"),
                        &format!("# Thread\n\n- {jan02} [Thu](10-00-00.md) *D*\n"),
                    ),
                    (&format!("{jan02}.thread.ai.md"), "thread ai Jan 02"),
                    (&format!("{jan02}.thread.human.md"), "thread human Jan 02"),
                    // Jan 06: email only (no .ai.md), next week
                    (
                        &format!("{jan06}.md"),
                        "Subject: Mon2\nFrom: E\nDate: Mon, 6 Jan 2025\n\nMon2.",
                    ),
                    (
                        &format!("{jan06}.thread.md"),
                        &format!("# Thread\n\n- {jan06} [Mon2](10-00-00.md) *E*\n"),
                    ),
                    // Feb 01: email only, next month
                    (
                        &format!("{feb01}.md"),
                        "Subject: Feb\nFrom: F\nDate: Sat, 1 Feb 2025\n\nFeb.",
                    ),
                    (
                        &format!("{feb01}.thread.md"),
                        &format!("# Thread\n\n- {feb01} [Feb](10-00-00.md) *F*\n"),
                    ),
                ],
            )
            .unwrap();

            fi.finish().unwrap();
        }

        // Use nth_word: 1 so the mock echoes the full prompt, allowing
        // us to verify which daily digests fed into weekly/monthly.
        let backend = Backend::Mock { nth_word: 1 };
        let result = run_pipeline(
            repo,
            git_ref,
            Some("2025/01/01"),
            None,
            20,
            Some(&backend),
            false,
        )
        .await
        .unwrap();

        // Emails summarized: Jan 06 and Feb 01 (only ones without .ai.md
        // in range).  Jan 02 already has .ai.md so only its daily digest
        // is generated, not a new email summary.
        assert_eq!(
            result.total_processed, 2,
            "should summarize Jan 06 + Feb 01 only; got {}",
            result.total_processed
        );

        let mut cat = CatFile::new(repo).unwrap();

        // --- Pre-since daily digests: NOT regenerated ---
        // Dec 30 daily digest still exists (was pre-existing)
        assert!(
            cat.get_str(&format!("{git_ref}:2024/12/30/digest.ai.md"))
                .is_some(),
            "Dec 30 daily digest should still exist"
        );

        // Dec 31 daily digest should still NOT exist (pre-since gap)
        assert!(
            cat.get_str(&format!("{git_ref}:2024/12/31/digest.ai.md"))
                .is_none(),
            "Dec 31 daily digest should NOT be generated (pre-since)"
        );

        // --- Post-since missing daily: generated ---
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/02/digest.ai.md"))
                .is_some(),
            "Jan 02 daily digest should be generated"
        );

        // --- Jan 06 email summarized + daily digest ---
        assert!(
            cat.get_str(&format!("{git_ref}:{jan06}.ai.md")).is_some(),
            "Jan 06 email should be summarized"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/06/digest.ai.md"))
                .is_some(),
            "Jan 06 daily digest should be generated"
        );

        // --- Weekly digests ---
        // Week ending Jan 05 (Mon Dec 30 – Sun Jan 05):
        // picks up dailies for Dec 30, Jan 01, Jan 02 (NOT Dec 31).
        let weekly_jan05 = cat
            .get_str(&format!("{git_ref}:2025/01/05/digest.weekly.human.md"))
            .expect("weekly digest for Jan 05 should exist");
        assert!(
            weekly_jan05.contains("2024/12/30"),
            "weekly Jan 05 should include Dec 30 daily"
        );
        assert!(
            weekly_jan05.contains("2025/01/01"),
            "weekly Jan 05 should include Jan 01 daily"
        );
        assert!(
            weekly_jan05.contains("2025/01/02"),
            "weekly Jan 05 should include Jan 02 daily"
        );
        assert!(
            !weekly_jan05.contains("2024/12/31"),
            "weekly Jan 05 must NOT include Dec 31 (no daily digest)"
        );

        // Week ending Jan 12 (Mon Jan 06 – Sun Jan 12):
        let weekly_jan12 = cat
            .get_str(&format!("{git_ref}:2025/01/12/digest.weekly.human.md"))
            .expect("weekly digest for Jan 12 should exist");
        assert!(
            weekly_jan12.contains("2025/01/06"),
            "weekly Jan 12 should include Jan 06 daily"
        );

        // --- Monthly digest ---
        let monthly_jan = cat
            .get_str(&format!("{git_ref}:2025/01/digest.monthly.human.md"))
            .expect("monthly digest for Jan should exist");
        // Monthly rolls up both weekly digests.
        assert!(
            monthly_jan.contains("2024/12/30"),
            "monthly Jan should include week of Jan 05 (contains Dec 30)"
        );
        assert!(
            monthly_jan.contains("2025/01/06"),
            "monthly Jan should include week of Jan 12 (contains Jan 06)"
        );

        // --- git fsck ---
        drop(cat);
        crate::git_util::tests::git(repo, &["fsck", "--strict"]);
    }

    /// Verify that a reply email's AI summary prompt includes the
    /// thread AI summary, the parent email's AI summary, and the
    /// email's own markdown body.
    ///
    /// Uses `Mock { nth_word: 1 }` which echoes every word of the
    /// user message, so the `.ai.md` output is the full prompt
    /// (whitespace-normalized).  We check that unique fragments
    /// from each input survive into the output.
    #[tokio::test]
    async fn test_reply_prompt_includes_parent_and_thread() {
        use crate::ai_backend::Backend;

        const ROOT: &str = "2025/03/01/10-00-00";
        const REPLY: &str = "2025/03/01/11-00-00";
        // Needs a second day so the first day boundary fires.
        const NEXT_DAY: &str = "2025/03/02/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        // Unique markers that we can search for in the echoed prompt.
        let thread_ai = "THREAD_MARKER_xyzzy42 accumulated thread context";
        let parent_ai = "PARENT_MARKER_plugh99 parent email summary text";
        let reply_body = "Subject: Re: topic\nFrom: Bob\n\
                          Date: Sat, 1 Mar 2025\n\n\
                          REPLY_MARKER_quux77 actual reply body";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            // Thread file lists root at depth 0, reply at depth 1.
            let thread_md = [
                "# Thread",
                "",
                &format!("- {ROOT} [topic](10-00-00.md) *Alice*"),
                &format!("  - {REPLY} [Re: topic](11-00-00.md) *Bob*"),
                "",
            ]
            .join("\n");

            fi.commit_with_symlinks(
                "seed",
                &[
                    (
                        &format!("{ROOT}.md"),
                        "Subject: topic\nFrom: Alice\nDate: Sat, 1 Mar 2025\n\nOriginal.",
                    ),
                    (&format!("{ROOT}.ai.md"), parent_ai),
                    (&format!("{ROOT}.thread.md"), &thread_md),
                    (&format!("{ROOT}.thread.ai.md"), thread_ai),
                    (&format!("{ROOT}.thread.human.md"), "thread human"),
                    (&format!("{ROOT}.human.md"), "root human"),
                    (&format!("{REPLY}.md"), reply_body),
                    // Next-day email so the day boundary fires.
                    (
                        &format!("{NEXT_DAY}.md"),
                        "Subject: other\nFrom: Carol\nDate: Sun, 2 Mar 2025\n\nUnrelated.",
                    ),
                    (
                        &format!("{NEXT_DAY}.thread.md"),
                        "# Thread\n\n- 2025/03/02/09-00-00 [other](09-00-00.md) *Carol*\n",
                    ),
                ],
                &[(&format!("{REPLY}.thread.md"), "10-00-00.thread.md")],
                &[],
            )
            .unwrap();

            fi.finish().unwrap();
        }

        // nth_word: 1 echoes the full user message back.
        let backend = Backend::Mock { nth_word: 1 };
        let result = run_pipeline(repo, git_ref, None, None, 10, Some(&backend), false)
            .await
            .unwrap();

        // The reply (and next-day standalone) should both be summarized.
        assert!(
            result.total_processed >= 2,
            "expected at least reply + next-day, got {}",
            result.total_processed
        );

        // Read the reply's AI summary from the repo.
        let mut cat = CatFile::new(repo).unwrap();
        let ai_md = cat
            .get_str(&format!("{git_ref}:{REPLY}.ai.md"))
            .expect("reply .ai.md should exist");

        // The echoed prompt must contain our unique markers.
        assert!(
            ai_md.contains("THREAD_MARKER_xyzzy42"),
            "prompt should include thread AI summary; got:\n{ai_md}"
        );
        assert!(
            ai_md.contains("PARENT_MARKER_plugh99"),
            "prompt should include parent AI summary; got:\n{ai_md}"
        );
        assert!(
            ai_md.contains("REPLY_MARKER_quux77"),
            "prompt should include reply email body; got:\n{ai_md}"
        );

        // Also check the structural prefixes survived.
        assert!(
            ai_md.contains("Thread AI summary:"),
            "prompt should have 'Thread AI summary:' header; got:\n{ai_md}"
        );
        assert!(
            ai_md.contains("Parent email AI summary:"),
            "prompt should have 'Parent email AI summary:' header; got:\n{ai_md}"
        );
    }

    /// Comprehensive end-to-end pipeline test.
    ///
    /// Timeline:
    ///   01/06: Alice (root A), Bob (reply→A), Carol (root B)
    ///   01/07: Alice_v2 (reply→A)
    ///   01/10: Dave (root C)
    ///   01/13: Eve (reply→A)
    ///   02/03: Frank (root D)
    ///
    /// Pre-existing state before pipeline:
    ///   Summaries: Alice✓ Bob✓ Carol✓ Alice_v2✓  (Dave✗ Eve✗ Frank✗)
    ///   Thread A: updated to post-v2 state AFTER the 01/06 daily digest
    ///   Daily digest 01/06: ✓ exists
    ///   Daily digest 01/07: ✗ MISSING (must be backfilled)
    ///
    /// Merge commit at tip has no Source-Commit trailer, testing fallback.
    #[tokio::test]
    async fn test_comprehensive_pipeline() {
        use crate::ai_backend::Backend;
        use crate::git_util::source_commit_from_ref;
        use crate::git_util::tests::git;

        const ALICE: &str = "2025/01/06/09-00-00";
        const BOB_REPLY: &str = "2025/01/06/10-00-00";
        const CAROL: &str = "2025/01/06/11-00-00";
        const ALICE_V2: &str = "2025/01/07/08-00-00";
        const DAVE: &str = "2025/01/10/09-00-00";
        const EVE_REPLY: &str = "2025/01/13/09-00-00";
        const FRANK: &str = "2025/02/03/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        // --- Seed commit 1: all emails with proper thread symlinks ---
        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();

            fi.commit_with_symlinks("seed: add all emails", &[
                // Alice (root of thread A, 01/06)
                (&format!("{ALICE}.md"),
                 "Subject: [PATCH] Fix frobnitz\nFrom: Alice\nDate: Mon, 6 Jan 2025\n\nPatch text here."),
                (&format!("{ALICE}.thread.md"), "# Thread: Fix frobnitz"),
                // Bob's reply to Alice (01/06)
                (&format!("{BOB_REPLY}.md"),
                 "Subject: Re: [PATCH] Fix frobnitz\nFrom: Bob\nDate: Mon, 6 Jan 2025\n\nLooks good, minor nit."),
                // Carol (root of thread B, 01/06)
                (&format!("{CAROL}.md"),
                 "Subject: [RFC] New merge strategy\nFrom: Carol\nDate: Mon, 6 Jan 2025\n\nNew recursive merge."),
                (&format!("{CAROL}.thread.md"), "# Thread: New merge strategy"),
                // Alice v2 (reply to A, 01/07)
                (&format!("{ALICE_V2}.md"),
                 "Subject: Re: [PATCH] Fix frobnitz\nFrom: Alice\nDate: Tue, 7 Jan 2025\n\nFixed the nit, v2."),
                // Dave (root of thread C, 01/10)
                (&format!("{DAVE}.md"),
                 "Subject: [PATCH] Update docs\nFrom: Dave\nDate: Fri, 10 Jan 2025\n\nDocs update."),
                (&format!("{DAVE}.thread.md"), "# Thread: Update docs"),
                // Eve's reply to Alice (01/13)
                (&format!("{EVE_REPLY}.md"),
                 "Subject: Re: [PATCH] Fix frobnitz\nFrom: Eve\nDate: Mon, 13 Jan 2025\n\nLGTM."),
                // Frank (root of thread D, 02/03)
                (&format!("{FRANK}.md"),
                 "Subject: [RFC] New feature\nFrom: Frank\nDate: Mon, 3 Feb 2025\n\nNew feature."),
                (&format!("{FRANK}.thread.md"), "# Thread: New feature"),
            ], &[
                // Symlinks: non-root emails point to thread A root
                (&format!("{BOB_REPLY}.thread.md"), "09-00-00.thread.md"),
                (&format!("{ALICE_V2}.thread.md"), "../06/09-00-00.thread.md"),
                (&format!("{EVE_REPLY}.thread.md"), "../../01/06/09-00-00.thread.md"),
            ], &[]).unwrap();

            // Seed commit 2: pre-existing email summaries and thread state.
            fi.commit(
                "seed: email summaries and thread state",
                &[
                    (&format!("{ALICE}.ai.md"), "SEED Alice ai summary"),
                    (&format!("{ALICE}.human.md"), "SEED Alice human summary"),
                    (&format!("{BOB_REPLY}.ai.md"), "SEED Bob ai summary"),
                    (&format!("{BOB_REPLY}.human.md"), "SEED Bob human summary"),
                    (&format!("{CAROL}.ai.md"), "SEED Carol ai summary"),
                    (&format!("{CAROL}.human.md"), "SEED Carol human summary"),
                    // Thread A pre-Alice-v2 state (includes Alice+Bob only)
                    (
                        &format!("{ALICE}.thread.ai.md"),
                        "BEFORE_V2 BEFORE_V2 BEFORE_V2 BEFORE_V2 BEFORE_V2",
                    ),
                    (
                        &format!("{ALICE}.thread.human.md"),
                        "SEED thread A human pre-v2",
                    ),
                    // Thread B
                    (&format!("{CAROL}.thread.ai.md"), "SEED thread B ai"),
                    (&format!("{CAROL}.thread.human.md"), "SEED thread B human"),
                ],
            )
            .unwrap();

            // Seed commit 3: daily digest for 01/06.
            fi.commit(
                "digestive: daily digest for 2025/01/06",
                &[
                    (
                        "2025/01/06/digest.human.md",
                        "SEED daily digest 01/06 human",
                    ),
                    ("2025/01/06/digest.ai.md", "SEED daily digest 01/06 ai"),
                ],
            )
            .unwrap();

            // Seed commit 4: Alice v2 summary + thread A updated to post-v2.
            fi.commit(
                "seed: Alice v2 summary\n\nSource-Commit: abc123",
                &[
                    (&format!("{ALICE_V2}.ai.md"), "SEED Alice v2 ai summary"),
                    (
                        &format!("{ALICE_V2}.human.md"),
                        "SEED Alice v2 human summary",
                    ),
                    // Thread A post-v2 state (DIFFERENT from pre-v2)
                    (
                        &format!("{ALICE}.thread.ai.md"),
                        "AFTER_V2 AFTER_V2 AFTER_V2 AFTER_V2 AFTER_V2",
                    ),
                    (
                        &format!("{ALICE}.thread.human.md"),
                        "SEED thread A human after v2",
                    ),
                ],
            )
            .unwrap();

            fi.finish().unwrap();
        }

        // Create a merge commit at the tip with no Source-Commit trailer.
        {
            let main_sha = git(repo, &["rev-parse", "refs/heads/main"]);
            let parent_sha = git(repo, &["rev-parse", "refs/heads/main~1"]);
            git(repo, &["update-ref", "refs/heads/side", &parent_sha]);
            let tree = git(repo, &["rev-parse", "refs/heads/main^{tree}"]);
            let merge = git(
                repo,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &main_sha,
                    "-p",
                    &parent_sha,
                    "-m",
                    "merge side branch",
                ],
            );
            git(repo, &["update-ref", "refs/heads/main", &merge]);
            git(repo, &["branch", "-D", "side"]);
        }

        // --- Run the pipeline ---
        let backend = Backend::Mock { nth_word: 5 };
        let result = run_pipeline(repo, git_ref, None, None, 5, Some(&backend), false)
            .await
            .unwrap();

        assert_eq!(
            result.total_processed, 3,
            "should summarize Dave, Eve, Frank (Alice/Bob/Carol/Alice_v2 are pre-existing)"
        );

        // --- Verify results ---
        let mut cat = CatFile::new(repo).unwrap();

        // 1. Pre-existing summaries must NOT be regenerated.
        assert_eq!(
            cat.get_str(&format!("{git_ref}:{ALICE}.ai.md")).unwrap(),
            "SEED Alice ai summary",
            "Alice summary should be preserved"
        );
        assert_eq!(
            cat.get_str(&format!("{git_ref}:{BOB_REPLY}.ai.md"))
                .unwrap(),
            "SEED Bob ai summary",
            "Bob summary should be preserved"
        );
        assert_eq!(
            cat.get_str(&format!("{git_ref}:{CAROL}.ai.md")).unwrap(),
            "SEED Carol ai summary",
            "Carol summary should be preserved"
        );
        assert_eq!(
            cat.get_str(&format!("{git_ref}:{ALICE_V2}.ai.md")).unwrap(),
            "SEED Alice v2 ai summary",
            "Alice v2 summary should be preserved"
        );

        // 2. New summaries must be generated for unsummarized emails.
        for dk in [DAVE, EVE_REPLY, FRANK] {
            let ai = cat.get_str(&format!("{git_ref}:{dk}.ai.md"));
            assert!(ai.is_some(), "missing .ai.md for {dk}");
            assert!(
                !ai.unwrap().starts_with("SEED"),
                "{dk} should have mock output, not seed data"
            );
            assert!(
                cat.get_str(&format!("{git_ref}:{dk}.human.md")).is_some(),
                "missing .human.md for {dk}"
            );
        }

        // 3. Pre-existing daily digest must NOT be regenerated.
        assert_eq!(
            cat.get_str(&format!("{git_ref}:2025/01/06/digest.human.md"))
                .unwrap(),
            "SEED daily digest 01/06 human",
            "01/06 daily digest should be preserved",
        );

        // 4. Missing daily digest for 01/07 must be backfilled.
        let digest_0107_human = cat.get_str(&format!("{git_ref}:2025/01/07/digest.human.md"));
        assert!(
            digest_0107_human.is_some(),
            "01/07 daily digest should be backfilled"
        );
        let digest_0107_ai = cat.get_str(&format!("{git_ref}:2025/01/07/digest.ai.md"));
        assert!(
            digest_0107_ai.is_some(),
            "01/07 daily AI digest should be backfilled"
        );

        // 5. Backfilled 01/07 digest must use pre-v2 thread state.
        let d07_human = digest_0107_human.unwrap();
        assert!(
            !d07_human.contains("AFTER_V2"),
            "01/07 digest should use pre-v2 thread state, but found post-v2 marker.\n\
             Content: {d07_human}"
        );

        // 6. No daily digest for gap days (no emails on 01/08, 01/09).
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/08/digest.human.md"))
                .is_none(),
            "01/08 should have no digest (no emails)"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/09/digest.human.md"))
                .is_none(),
            "01/09 should have no digest (no emails)"
        );

        // 7. Daily digests generated for days with new emails.
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/10/digest.human.md"))
                .is_some(),
            "01/10 daily digest should exist"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/13/digest.human.md"))
                .is_some(),
            "01/13 daily digest should exist"
        );

        // 8. Last day (02/03) now HAS a daily digest (UTC midnight
        //    long past when the test runs).
        assert!(
            cat.get_str(&format!("{git_ref}:2025/02/03/digest.human.md"))
                .is_some(),
            "last day (02/03) should have a daily digest"
        );

        // 9. Weekly digest for week 1 (01/06-01/12).
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/12/digest.weekly.human.md"))
                .is_some(),
            "week 1 (01/12) should have a weekly digest"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/12/digest.weekly.ai.md"))
                .is_some(),
            "week 1 (01/12) should have a weekly AI digest"
        );

        // 10. Weekly digest for week 2 (01/13-01/19).
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/19/digest.weekly.human.md"))
                .is_some(),
            "week 2 (01/19) should have a weekly digest"
        );

        // 11. Monthly digest for January.
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/digest.monthly.human.md"))
                .is_some(),
            "January should have a monthly digest"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/digest.monthly.ai.md"))
                .is_some(),
            "January should have a monthly AI digest"
        );

        // 12. Weekly/monthly for the last period are now generated
        //     because UTC midnight is long past.
        assert!(
            cat.get_str(&format!("{git_ref}:2025/02/09/digest.weekly.human.md"))
                .is_some(),
            "week of 02/03 should have a weekly digest"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/02/digest.monthly.human.md"))
                .is_some(),
            "February should have a monthly digest"
        );

        // 13. git fsck --strict: no NUL bytes or object corruption.
        drop(cat);
        crate::git_util::tests::git(repo, &["fsck", "--strict"]);

        // 14. Source-Commit propagation through the merge commit.
        let source = source_commit_from_ref(repo, git_ref);
        assert_eq!(
            source.as_deref(),
            Some("abc123"),
            "source_commit_from_ref should find trailer despite merge at tip"
        );

        // 15. Idempotent resume: running again should produce no new work.
        let result2 = run_pipeline(repo, git_ref, None, None, 5, Some(&backend), false)
            .await
            .unwrap();
        assert_eq!(
            result2.total_processed, 0,
            "all emails should already be summarized on second run"
        );
    }

    /// A deadline that has already passed must cause `run` to exit
    /// before processing any email, leave `total_processed` at zero,
    /// and skip the midnight flush so that no partial daily digest is
    /// written.  This is the `--max-runtime 0s` smoke test from the
    /// soft-deadline finding's acceptance criteria.
    #[tokio::test]
    async fn run_skips_all_work_when_deadline_already_passed() {
        use crate::ai_backend::Backend;

        const E1: &str = "2025/01/06/09-00-00";
        const E2: &str = "2025/01/06/10-00-00";
        const E3: &str = "2025/01/07/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();
            fi.commit_with_symlinks(
                "seed",
                &[
                    (
                        &format!("{E1}.md"),
                        "Subject: A\nFrom: A\nDate: Mon, 6 Jan 2025\n\nbody",
                    ),
                    (&format!("{E1}.thread.md"), "# T1"),
                    (
                        &format!("{E2}.md"),
                        "Subject: B\nFrom: B\nDate: Mon, 6 Jan 2025\n\nbody",
                    ),
                    (
                        &format!("{E3}.md"),
                        "Subject: C\nFrom: C\nDate: Tue, 7 Jan 2025\n\nbody",
                    ),
                    (&format!("{E3}.thread.md"), "# T2"),
                ],
                &[(&format!("{E2}.thread.md"), "09-00-00.thread.md")],
                &[],
            )
            .unwrap();
            fi.finish().unwrap();
        }

        let backend = Backend::Mock { nth_word: 3 };
        let mut d = Digestive::new(repo, git_ref, 5, Some(&backend), false).unwrap();
        d = d.with_deadline(std::time::Instant::now());
        d.run(None, None).await.unwrap();
        let result = d.finish().unwrap();

        assert_eq!(
            result.total_processed, 0,
            "no emails should be summarized when deadline is already past",
        );

        let mut cat = CatFile::new(repo).unwrap();
        assert!(
            cat.get_str(&format!("{git_ref}:{E1}.ai.md")).is_none(),
            "E1 must not have been summarized"
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/06/digest.human.md"))
                .is_none(),
            "no daily digest should be written: midnight flush must be \
             suppressed when the deadline forced an early exit",
        );
        assert!(
            cat.get_str(&format!("{git_ref}:2025/01/07/digest.human.md"))
                .is_none(),
            "no daily digest should be written for the partially processed day",
        );
    }

    /// A deadline far in the future is a no-op: the pipeline must
    /// behave identically to a run with no deadline set.  This is the
    /// `--max-runtime <large>` acceptance criterion: opting in to the
    /// feature does not change behavior when the deadline does not
    /// fire.
    #[tokio::test]
    async fn run_with_distant_deadline_processes_all_emails() {
        use crate::ai_backend::Backend;

        const E1: &str = "2025/01/06/09-00-00";
        const E2: &str = "2025/01/06/10-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();
            fi.commit_with_symlinks(
                "seed",
                &[
                    (
                        &format!("{E1}.md"),
                        "Subject: A\nFrom: A\nDate: Mon, 6 Jan 2025\n\nbody",
                    ),
                    (&format!("{E1}.thread.md"), "# T1"),
                    (
                        &format!("{E2}.md"),
                        "Subject: B\nFrom: B\nDate: Mon, 6 Jan 2025\n\nbody",
                    ),
                ],
                &[(&format!("{E2}.thread.md"), "09-00-00.thread.md")],
                &[],
            )
            .unwrap();
            fi.finish().unwrap();
        }

        let backend = Backend::Mock { nth_word: 3 };
        let mut d = Digestive::new(repo, git_ref, 5, Some(&backend), false).unwrap();
        d = d.with_deadline(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        d.run(None, None).await.unwrap();
        let result = d.finish().unwrap();

        assert_eq!(
            result.total_processed, 2,
            "both emails should be summarized when deadline is far in the future",
        );
    }
}
