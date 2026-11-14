//! Batch AI summarization of emails in a bare git repository.
//!
//! Streams ls-tree output, looks up thread context lazily, generates
//! summaries via an AI backend, and writes results back to the
//! repository via fast-import.

use std::collections::BTreeMap;
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
const DAILY_DIGEST_AUDIT_AGENT: &str = include_str!("../prompts/git-daily-digest-audit.md");
const DAILY_DELTA_AGENT: &str = include_str!("../prompts/git-daily-delta.md");
const DAILY_SELECT_AGENT: &str = include_str!("../prompts/git-daily-select.md");
const DAILY_REVISE_AGENT: &str = include_str!("../prompts/git-daily-revise.md");

/// Per-thread data accumulated for a single day's digest.
pub struct ThreadDayActivity {
    /// Thread root date-key.
    pub root_dk: String,
    /// Thread AI summary from *before* today's emails (None if new thread).
    pub thread_ai_before: Option<String>,
    /// Source text describing the topic as of this day's latest email.
    pub root_source_excerpt: Option<String>,
    /// Today's emails and complete source text, in chronological order.
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
            let latest_dk = emails
                .iter()
                .map(|email| email.dk.as_str())
                .max()
                .expect("a grouped thread has at least one email");
            let root_source_excerpt = topic_source_evidence(&root_dk, latest_dk, git_ref, cat);
            let wc_status = lookup_wc_status(&root_dk, git_ref, cat, &wc_map);
            ThreadDayActivity {
                root_dk,
                thread_ai_before,
                root_source_excerpt,
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
    let source_is_short = authored_email_text(&parsed.body).chars().count() <= 300;
    (
        author,
        Some(format!("Subject: {}\n\n{}", parsed.subject, parsed.body)),
        source_is_short,
    )
}

/// Use the most recent cover letter or standalone patch available at
/// the cutoff, rather than a root whose topic may since have changed.
fn topic_source_evidence(
    root_dk: &str,
    latest_dk: &str,
    git_ref: &str,
    cat: &mut impl BlobRead,
) -> Option<String> {
    let source_dk = thread_file::load_from_repo(cat, git_ref, root_dk)
        .and_then(|(_, tree)| {
            tree.date_keys()
                .filter(|dk| *dk <= latest_dk)
                .filter(|dk| {
                    tree.node_of(dk)
                        .is_some_and(|node| is_topic_post(&node.subject))
                })
                .max()
                .map(str::to_string)
        })
        .or_else(|| (root_dk <= latest_dk).then(|| root_dk.to_string()))?;
    let md = cat.get_str(&format!("{git_ref}:{source_dk}.md"))?;
    let parsed = rag_parse::parse_email(&md);
    let authored = authored_email_text(&parsed.body);
    let excerpt = bounded_excerpt(&authored, 2000);
    Some(format!("Subject: {}\n\n{excerpt}", parsed.subject))
}

fn is_topic_post(subject: &str) -> bool {
    let Some((tags, _)) = subject
        .strip_prefix('[')
        .and_then(|text| text.split_once(']'))
    else {
        return false;
    };
    if !tags.split_whitespace().any(|tag| {
        tag.split('/')
            .any(|part| part.eq_ignore_ascii_case("PATCH"))
    }) {
        return false;
    }
    match tags
        .split_whitespace()
        .find(|tag| tag.starts_with(|c: char| c.is_ascii_digit()) && tag.contains('/'))
    {
        Some(part) => part.split_once('/').is_some_and(|(number, total)| {
            number.parse::<usize>().ok() == Some(0) && total.parse::<usize>().is_ok_and(|n| n > 0)
        }),
        None => true,
    }
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
/// Extract developments from each thread's complete same-day source
/// before presenting the resulting topic packets to the editor.
pub async fn generate_daily_digest(
    day: &str,
    threads: &[ThreadDayActivity],
    email_count: usize,
    backend: &Backend,
) -> Result<DayDigestOutput> {
    let thread_count = threads.len();

    eprintln!(
        "[digestive] extracting daily deltas for {day} \
         ({email_count} emails, {thread_count} threads) ...",
    );

    let mut root_deltas = Vec::new();
    for thread in threads {
        root_deltas.extend(extract_thread_deltas(day, thread, backend).await?);
    }
    let packets = build_topic_packets(day, threads, &root_deltas);
    if packets.is_empty() {
        anyhow::bail!("no same-day thread delta survived extraction for {day}");
    }
    let evidence = render_topic_packets(&packets);
    trace_digest_attempt("editor", 0, "evidence.txt", &evidence)?;
    eprintln!("[digestive] generating daily digest for {day} ...");

    let human = generate_digest_mode(day, &HUMAN_DIGEST_MODE, &packets, backend).await?;
    let ai = generate_digest_mode(day, &AI_DIGEST_MODE, &packets, backend).await?;

    Ok(DayDigestOutput { human, ai })
}

async fn extract_thread_deltas(
    day: &str,
    thread: &ThreadDayActivity,
    backend: &Backend,
) -> Result<Vec<RootDelta>> {
    for email in &thread.email_summaries {
        anyhow::ensure!(
            email.source_excerpt.is_some(),
            "missing source email {} for daily delta extraction",
            email.dk,
        );
    }
    let threads = std::slice::from_ref(thread);
    let input = build_daily_digest_user_msg(day, threads, thread.email_summaries.len());
    let label = format!("thread-{}", thread.root_dk.replace('/', "-"));
    let mut correction = String::new();
    for attempt in 1..=2 {
        let prompt = format!("{input}{correction}");
        trace_digest_attempt(&label, attempt, "input.txt", &prompt)?;
        let deltas = backend
            .chat_with_options(DAILY_DELTA_AGENT, &prompt, Some(0.0))
            .await
            .with_context(|| format!("daily delta extraction failed for {}", thread.root_dk))?;
        trace_digest_attempt(&label, attempt, "output.txt", &deltas)?;
        if is_mock(backend) {
            return Ok(vec![RootDelta {
                root_dk: thread.root_dk.clone(),
                bullets: vec![deltas],
            }]);
        }
        match parse_root_deltas(&deltas, threads) {
            Ok(deltas) => return Ok(deltas),
            Err(err) if attempt == 1 => {
                eprintln!(
                    "[digestive] {day} {} extraction rejected: {err:#}",
                    thread.root_dk
                );
                correction = format!(
                    "\n\nMANDATORY FORMAT CORRECTION:\n\
                     The previous output was rejected: {err:#}\n\
                     Emit only the requested bullets. Copy each email's \
                     date-key exactly from its input header; never \
                     construct or alter a date-key.\n",
                );
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the bounded extraction loop always returns")
}

/// One thread root's same-day delta bullets, recovered from the merged
/// output of the two delta extractors.
#[derive(Debug, PartialEq)]
struct RootDelta {
    root_dk: String,
    bullets: Vec<String>,
}

/// One topic as the final editor sees it: an opaque ID, the thread root
/// author's own background words, and today's delta bullets, colocated
/// so that no cross-topic lookup is possible.
struct TopicPacket {
    id: String,
    current_subject: Option<String>,
    background: Option<String>,
    bullets: Vec<String>,
    source: String,
}

/// Bind each delta to its cited email's source metadata, not to the
/// model's thread or author labels.
fn parse_root_deltas(raw: &str, threads: &[ThreadDayActivity]) -> Result<Vec<RootDelta>> {
    let mut by_thread: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut current: Option<usize> = None;

    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with("```")
            || line.chars().all(|c| c == '-')
            || is_delta_sentinel(line)
            || (current.is_none() && is_delta_preamble(line))
        {
            continue;
        }
        if let Some(dk) = line.strip_prefix("Thread root:") {
            let dk = dk.trim();
            threads
                .iter()
                .find(|thread| thread.root_dk == dk)
                .with_context(|| format!("delta extractor named unknown thread root `{dk}`"))?;
            current = None;
            continue;
        }
        if line.eq_ignore_ascii_case("New today:") {
            continue;
        }
        // The extractor sometimes drops the "- " from every bullet; a
        // line that opens with a citation of a known email is one anyway.
        let bullet = line.strip_prefix("- ").or_else(|| {
            let (header, _) = line.strip_prefix('[')?.split_once(']')?;
            cited_email(header, threads).is_ok().then_some(line)
        });
        match bullet {
            Some(bullet) if raw_line.starts_with('-') || bullet.starts_with('[') => {
                let (header, text) = bullet
                    .strip_prefix('[')
                    .and_then(|text| text.split_once(']'))
                    .context("delta extractor emitted a bullet without an email citation")?;
                let (index, email) = cited_email(header, threads)?;
                let dk = &email.dk;
                let text = text.trim();
                anyhow::ensure!(!text.is_empty(), "empty delta for source email `{dk}`");
                let citation = match &email.from {
                    Some(author) => format!("[{dk} by {author}]"),
                    None => format!("[{dk}]"),
                };
                by_thread
                    .entry(index)
                    .or_default()
                    .push(format!("{citation} {text}"));
                current = Some(index);
            }
            _ => {
                let last = current
                    .and_then(|index| by_thread.get_mut(&index))
                    .and_then(|bullets| bullets.last_mut())
                    .with_context(|| format!("delta extractor emitted stray line `{line}`"))?;
                last.push(' ');
                last.push_str(line);
            }
        }
    }

    Ok(by_thread
        .into_iter()
        .map(|(index, mut bullets)| {
            // Keep clauses from the same email in their original order.
            bullets.sort_by(|a, b| {
                a.split_whitespace()
                    .next()
                    .cmp(&b.split_whitespace().next())
            });
            RootDelta {
                root_dk: threads[index].root_dk.clone(),
                bullets,
            }
        })
        .collect())
}

/// The thread index and email that a bullet's `[date-key]` or
/// `[date-key by Author]` header names.  Only the date-key counts; the
/// author comes from the source email.
fn cited_email<'a>(
    header: &str,
    threads: &'a [ThreadDayActivity],
) -> Result<(usize, &'a DayEmailEvidence)> {
    let dk = header
        .split_once(" by ")
        .map_or(header, |(dk, _)| dk)
        .trim();
    threads
        .iter()
        .enumerate()
        .find_map(|(index, thread)| {
            thread
                .email_summaries
                .iter()
                .find(|email| email.dk == dk)
                .map(|email| (index, email))
        })
        .with_context(|| format!("delta extractor cited unknown source email `{dk}`"))
}

fn is_delta_sentinel(line: &str) -> bool {
    let line = line.strip_prefix("- ").unwrap_or(line);
    line.eq_ignore_ascii_case("None.")
        || (line.starts_with("No noteworthy") && line.ends_with("deltas."))
}

fn is_delta_preamble(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.starts_with("here are ") && line.contains("deltas")
}

/// Pair each root that has same-day deltas with its root author's
/// background and an opaque, deterministic topic ID.  A root without
/// deltas has no packet, and therefore no way into the digest.
fn build_topic_packets(
    day: &str,
    threads: &[ThreadDayActivity],
    deltas: &[RootDelta],
) -> Vec<TopicPacket> {
    deltas
        .iter()
        .filter(|delta| !delta.bullets.is_empty())
        .enumerate()
        .map(|(n, delta)| {
            let thread = threads
                .iter()
                .find(|thread| thread.root_dk == delta.root_dk)
                .expect("delta root was validated against source threads");
            TopicPacket {
                id: format!("T{:02}", n + 1),
                current_subject: thread.email_summaries.iter().rev().find_map(|email| {
                    email
                        .source_excerpt
                        .as_deref()?
                        .lines()
                        .next()?
                        .strip_prefix("Subject: ")
                        .map(str::to_string)
                }),
                background: thread.root_source_excerpt.clone(),
                bullets: delta.bullets.clone(),
                source: build_daily_digest_user_msg(
                    day,
                    std::slice::from_ref(thread),
                    thread.email_summaries.len(),
                ),
            }
        })
        .collect()
}

fn render_topic_packets<'a>(packets: impl IntoIterator<Item = &'a TopicPacket>) -> String {
    let mut out = String::new();
    for TopicPacket {
        id,
        current_subject,
        background,
        bullets,
        ..
    } in packets
    {
        out.push_str(&format!("---\nTOPIC {id}\n\n"));
        if let Some(subject) = current_subject {
            out.push_str(&format!("CURRENT SOURCE SUBJECT FOR {id}: {subject}\n\n"));
        }
        match background {
            Some(background) => out.push_str(&format!(
                "BACKGROUND FOR {id} -- ORIGINAL TOPIC-DEFINING SOURCE TEXT. \
                 THEY SAY WHAT THIS TOPIC IS, NOT WHAT CHANGED TODAY OR \
                 HOW IT IS GOING:\n\n{background}\n\n",
            )),
            None => out.push_str(&format!(
                "NO BACKGROUND FOR {id} -- WRITE ONLY FROM ITS DELTAS.\n\n",
            )),
        }
        out.push_str(&format!(
            "TODAY'S VERIFIED DELTAS FOR {id}, IN CHRONOLOGICAL ORDER:\n",
        ));
        for bullet in bullets {
            out.push_str(&format!("- {bullet}\n"));
        }
        out.push('\n');
    }
    out
}

struct DigestMode {
    label: &'static str,
    min_words: usize,
    max_words: usize,
    notable_count: usize,
    brief_limit: usize,
}

const HUMAN_DIGEST_MODE: DigestMode = DigestMode {
    label: "human",
    min_words: 500,
    max_words: 900,
    notable_count: 3,
    brief_limit: 8,
};

const AI_DIGEST_MODE: DigestMode = DigestMode {
    label: "ai",
    min_words: 800,
    max_words: 1400,
    notable_count: 5,
    brief_limit: 12,
};

#[derive(Debug)]
struct TopicSelection {
    notable: Vec<String>,
    brief: Vec<String>,
}

impl TopicSelection {
    fn from_ranking(mut ranking: Vec<String>, mode: &DigestMode) -> Self {
        let mut brief = ranking.split_off(mode.notable_count.min(ranking.len()));
        brief.truncate(mode.brief_limit);
        Self {
            notable: ranking,
            brief,
        }
    }
}

fn response_json(raw: &str) -> Result<&str> {
    let raw = raw.trim();
    if raw.starts_with("```") {
        raw.split_once('\n')
            .and_then(|(_, json)| json.strip_suffix("```"))
            .map(str::trim)
            .context("response has an incomplete JSON fence")
    } else {
        Ok(raw)
    }
}

fn parse_topic_ranking(raw: &str, packets: &[TopicPacket]) -> Result<Vec<String>> {
    let ranking: Vec<String> = serde_json::from_str(response_json(raw)?)
        .context("topic ranking must be a JSON ID array")?;
    anyhow::ensure!(
        ranking.len() >= 3,
        "topic ranking must select at least three topics"
    );
    for (index, id) in ranking.iter().enumerate() {
        anyhow::ensure!(
            packets.iter().any(|packet| packet.id == *id),
            "unknown ranked topic {id}"
        );
        anyhow::ensure!(
            !ranking[..index].contains(id),
            "duplicate ranked topic {id}"
        );
    }
    Ok(ranking)
}

async fn select_digest_topics(
    day: &str,
    mode: &DigestMode,
    packets: &[TopicPacket],
    backend: &Backend,
) -> Result<TopicSelection> {
    if is_mock(backend) {
        return Ok(TopicSelection {
            notable: packets.iter().map(|packet| packet.id.clone()).collect(),
            brief: Vec::new(),
        });
    }
    anyhow::ensure!(
        packets.len() >= 3,
        "daily digest needs at least three source-grounded topics",
    );
    let input = format!(
        "Date: {day}\nMode: {}\nThe editor has room for {} notable topics and \
         at most {} in-brief topics. Rank the supplied topics by their \
         significance today. Code will take the leading IDs that fit.\n\n{}",
        mode.label,
        mode.notable_count,
        mode.brief_limit,
        render_topic_packets(packets),
    );
    let mut correction = String::new();
    for attempt in 1..=2 {
        let prompt = format!("{input}{correction}");
        trace_digest_attempt(mode.label, attempt, "selection-input.txt", &prompt)?;
        let raw = backend
            .chat_with_options(DAILY_SELECT_AGENT, &prompt, Some(0.0))
            .await
            .context("daily topic selection failed")?;
        trace_digest_attempt(mode.label, attempt, "selection-output.txt", &raw)?;
        match parse_topic_ranking(&raw, packets) {
            Ok(ranking) => return Ok(TopicSelection::from_ranking(ranking, mode)),
            Err(err) if attempt == 1 => {
                eprintln!("[digestive] topic selection rejected: {err:#}");
                correction = format!(
                    "\n\nThe ranking was rejected: {err:#}\n\
                    Return only a JSON array of distinct supplied topic IDs."
                );
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the bounded selection loop always returns")
}

fn selection_issue(digest: &str, selection: &TopicSelection) -> Option<String> {
    let notable: Vec<_> = audit_sections(digest)
        .into_iter()
        .filter(|section| {
            section
                .lines
                .first()
                .is_some_and(|(_, line)| line.trim_start().starts_with("### "))
        })
        .filter_map(|section| section.topic_id)
        .collect();
    if notable.len() == selection.notable.len()
        && selection
            .notable
            .iter()
            .all(|id| notable.contains(&id.as_str()))
    {
        None
    } else {
        Some(format!(
            "notable headings must use exactly these selected IDs: {}",
            selection.notable.join(", ")
        ))
    }
}

const DIGEST_H2_SEQUENCE: [&str; 3] = ["the day in brief", "notable threads", "in brief"];

const GENERIC_LABELS: [&str; 10] = [
    "key details",
    "key technical details",
    "key developments",
    "key improvements",
    "status",
    "impact",
    "what changed",
    "why it matters",
    "review highlights",
    "current status",
];

const MAX_ISSUES: usize = 8;

/// Drafts rejected by the structural checks and drafts flagged by the
/// factual audit are limited separately: a writer that keeps missing
/// the word window must not leave the audit's findings unrepaired.
const MAX_STRUCTURAL_REJECTIONS: usize = 4;
const MAX_AUDIT_ROUNDS: usize = 6;

async fn generate_digest_mode(
    day: &str,
    mode: &DigestMode,
    packets: &[TopicPacket],
    backend: &Backend,
) -> Result<String> {
    let label = mode.label;
    let selection = select_digest_topics(day, mode, packets, backend).await?;
    let ids: Vec<_> = selection
        .notable
        .iter()
        .chain(&selection.brief)
        .map(String::as_str)
        .collect();
    let selected = ids.iter().map(|id| {
        packets
            .iter()
            .find(|packet| packet.id == *id)
            .expect("selected ID was validated against source packets")
    });
    let user_msg = format!(
        "Date: {day}\n\nSELECTED TOPICS ONLY:\n\
         Use exactly these IDs for notable headings: {}.\n\
         Other entries may only use these in-brief IDs: {}.\n\
         This is a selected subset, not an inventory of all activity.\n\n{}",
        selection.notable.join(", "),
        selection.brief.join(", "),
        render_topic_packets(selected),
    );
    let mut correction = String::new();
    let mut issues = Vec::new();
    let mut revision: Option<(String, Vec<String>)> = None;
    // Sources are fixed for this mode's retries; only identical accepted text is reusable.
    let mut approved = HashMap::new();
    let mut structural_rejections = 0;
    let mut audit_rounds = 0;

    for attempt in 1.. {
        let prompt = format!(
            "Mode: {label}\n\n{user_msg}\n\n{}{correction}",
            output_contract(mode, &ids),
        );
        let raw = if let Some((draft, findings)) = revision.take() {
            revise_digest_items(
                &draft,
                &findings,
                packets,
                mode,
                backend,
                &format!("{label}-{attempt}"),
            )
            .await?
        } else {
            trace_digest_attempt(label, attempt, "input.txt", &prompt)?;
            backend
                .chat_with_options(DAILY_DIGEST_AGENT, &prompt, Some(0.0))
                .await
                .with_context(|| format!("daily digest ({label}) failed"))?
        };
        let digest = normalize_topic_tags(&normalize_typography(&strip_digest_links(
            &summarize::normalize_headings(&raw),
        )));
        trace_digest_attempt(label, attempt, "draft.md", &digest)?;
        if is_mock(backend) {
            return Ok(strip_topic_tags(&digest));
        }
        let mut structural_issues = validate_digest(&digest, mode, &ids, day);
        if let Some(issue) = selection_issue(&digest, &selection) {
            structural_issues.push(issue);
            structural_issues.truncate(MAX_ISSUES);
        }
        if structural_issues.is_empty() {
            audit_rounds += 1;
            issues = audit_digest(
                &digest,
                packets,
                &mut approved,
                backend,
                &format!("{label}-{attempt}"),
            )
            .await?;
            if issues.is_empty() {
                return Ok(drop_empty_brief_section(&strip_topic_tags(&digest)));
            }
            revision = Some((digest.clone(), issues.clone()));
        } else {
            structural_rejections += 1;
            issues = structural_issues;
        }
        trace_digest_attempt(label, attempt, "issues.txt", &issues.join("\n"))?;
        eprintln!(
            "[digestive] {day} {label} digest rejected:\n{}",
            issues.join("\n"),
        );
        if structural_rejections == MAX_STRUCTURAL_REJECTIONS || audit_rounds == MAX_AUDIT_ROUNDS {
            break;
        }
        correction = format!("\n\n{}", correction_block(&issues));
    }

    anyhow::bail!(
        "daily digest ({label}) for {day} failed validation or factual audit \
         after {} attempts:\n{}",
        structural_rejections + audit_rounds,
        issues.join("\n"),
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisedItem {
    title: String,
    body: String,
}

fn digest_word_count(md: &str) -> usize {
    strip_topic_tags(md)
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .flat_map(str::split_whitespace)
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count()
}

fn render_revised_item(section: &AuditSection<'_>, raw: &str) -> Result<String> {
    let item: RevisedItem = serde_json::from_str(response_json(raw)?)
        .context("item revision must contain JSON title and body strings")?;
    anyhow::ensure!(!item.body.trim().is_empty(), "item revision has no body");
    let body = item.body.trim();
    match section.topic_id {
        None => Ok(format!("## The day in brief\n\n{body}\n\n")),
        Some(id) => {
            let title = item.title.trim();
            anyhow::ensure!(
                !title.is_empty() && !title.contains(['\n', '\r']),
                "item revision needs a one-line title"
            );
            if section.lines[0].1.trim_start().starts_with("### ") {
                Ok(format!("### [{id}] {title}\n\n{body}\n\n"))
            } else {
                Ok(format!("- [{id}] **{title}**: {body}\n\n"))
            }
        }
    }
}

async fn revise_digest_items(
    digest: &str,
    issues: &[String],
    packets: &[TopicPacket],
    mode: &DigestMode,
    backend: &Backend,
    trace_label: &str,
) -> Result<String> {
    let sections = audit_sections(digest);
    let lines: Vec<_> = digest.lines().collect();
    let mut out = String::new();
    let mut cursor = 0;
    let mut total = digest_word_count(digest);
    let mut handled = 0;
    for section in &sections {
        let id = section.topic_id.unwrap_or("T00");
        let findings: Vec<_> = issues
            .iter()
            .filter(|issue| issue.starts_with(&format!("{id}: ")))
            .collect();
        if findings.is_empty() {
            continue;
        }
        handled += findings.len();
        let evidence = match section.topic_id {
            Some(id) => packets
                .iter()
                .find(|packet| packet.id == id)
                .with_context(|| format!("missing original source for revision {id}"))?
                .source
                .clone(),
            None => sections
                .iter()
                .filter(|section| section.topic_id.is_some())
                .flat_map(|section| section.lines.iter().map(|(_, line)| *line))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let old = section
            .lines
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        let old_words = digest_word_count(&old);
        let other_words = total - old_words;
        let min_words = mode.min_words.saturating_sub(other_words);
        let max_words = mode.max_words.saturating_sub(other_words);
        let base = format!(
            "Revise only item {id}. Aim for {old_words} words including its title; \
                 the rendered item must contain {min_words}-{max_words} words to \
                 preserve the overall digest budget.\n\nORIGINAL SOURCE EVIDENCE:\n\n\
                 {evidence}\n\nFINDINGS TO ADDRESS:\n{}\n\nEXISTING ITEM:\n\n{old}",
            findings
                .iter()
                .map(|finding| finding.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let label = format!("{trace_label}-revise-{id}");
        let mut correction = String::new();
        let mut replacement = None;
        for attempt in 1..=2 {
            let prompt = format!("{base}{correction}");
            trace_digest_attempt(&label, attempt, "input.txt", &prompt)?;
            let raw = backend
                .chat_with_options(DAILY_REVISE_AGENT, &prompt, Some(0.0))
                .await
                .with_context(|| format!("source-backed revision failed for {id}"))?;
            trace_digest_attempt(&label, attempt, "output.txt", &raw)?;
            let candidate = render_revised_item(section, &raw).and_then(|rendered| {
                let rendered =
                    normalize_topic_tags(&normalize_typography(&strip_digest_links(&rendered)));
                let words = digest_word_count(&rendered);
                anyhow::ensure!(
                    (min_words..=max_words).contains(&words),
                    "revised item has {words} words; required {min_words}-{max_words}"
                );
                Ok(rendered)
            });
            match candidate {
                Ok(text) => {
                    replacement = Some(text);
                    break;
                }
                Err(err) if attempt == 1 => {
                    eprintln!("[digestive] revision of {id} rejected: {err:#}");
                    correction = format!(
                        "\n\nCorrect this revision error: {err:#}\nReturn only the requested JSON object."
                    );
                }
                Err(err) => return Err(err),
            }
        }
        let replacement =
            replacement.expect("bounded revision either succeeds or returns an error");
        total = other_words + digest_word_count(&replacement);
        let start = section.lines[0].0 - 1;
        let end = section
            .lines
            .last()
            .expect("an audit section is nonempty")
            .0;
        for line in &lines[cursor..start] {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&replacement);
        cursor = end;
    }
    anyhow::ensure!(
        handled == issues.len() && handled > 0,
        "revision findings did not map to the current digest items"
    );
    for line in &lines[cursor..] {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn trace_digest_attempt(label: &str, attempt: usize, suffix: &str, content: &str) -> Result<()> {
    let Some(dir) = std::env::var_os("DIGESTIVE_TRACE_DIR") else {
        return Ok(());
    };
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).context("create daily digest trace directory")?;
    std::fs::write(dir.join(format!("{label}-{attempt}-{suffix}")), content)
        .context("write daily digest trace")
}

struct AuditSection<'a> {
    topic_id: Option<&'a str>,
    lines: Vec<(usize, &'a str)>,
}

fn audit_sections(digest: &str) -> Vec<AuditSection<'_>> {
    let mut sections: Vec<AuditSection<'_>> = Vec::new();
    let mut current = None;
    let mut in_brief = false;
    let mut fenced = false;
    for (index, line) in digest.lines().enumerate() {
        let trimmed = line.trim();
        if !fenced {
            if let Some(heading) = trimmed.strip_prefix("## ") {
                current = None;
                let heading = label_key(heading);
                in_brief = heading == "in brief";
                if heading == "the day in brief" {
                    sections.push(AuditSection {
                        topic_id: None,
                        lines: Vec::new(),
                    });
                    current = Some(sections.len() - 1);
                }
            }
            let item = trimmed.strip_prefix("### ").or_else(|| {
                if in_brief {
                    trimmed.strip_prefix("- ")
                } else {
                    None
                }
            });
            if let Some(topic_id) = item.and_then(|text| split_topic_tag(text).0) {
                sections.push(AuditSection {
                    topic_id: Some(topic_id),
                    lines: Vec::new(),
                });
                current = Some(sections.len() - 1);
            }
        }
        if let Some(current) = current {
            sections[current].lines.push((index + 1, line));
        }
        if trimmed.starts_with("```") {
            fenced = !fenced;
        }
    }
    sections
}

async fn audit_digest(
    digest: &str,
    packets: &[TopicPacket],
    approved: &mut HashMap<String, String>,
    backend: &Backend,
    trace_label: &str,
) -> Result<Vec<String>> {
    let sections = audit_sections(digest);
    let mut issues = Vec::new();
    let mut verified = String::from("VERIFIED DIGEST ITEMS:\n\n");
    for section in &sections {
        let Some(id) = section.topic_id else { continue };
        let packet = packets
            .iter()
            .find(|packet| packet.id == id)
            .with_context(|| format!("no source packet for audit topic {id}"))?;
        let text = section
            .lines
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        if approved.get(id) != Some(&text) {
            let findings = audit_section(id, &packet.source, section, backend, trace_label).await?;
            if !findings.is_empty() {
                issues.extend(findings);
                if issues.len() >= MAX_ISSUES {
                    issues.truncate(MAX_ISSUES);
                    return Ok(issues);
                }
                continue;
            }
            approved.insert(id.to_string(), text.clone());
        }
        verified.push_str(&format!("TOPIC {id}\n{text}\n\n"));
    }
    if !issues.is_empty() {
        return Ok(issues);
    }
    if let Some(overview) = sections.iter().find(|section| section.topic_id.is_none()) {
        return audit_section("T00", &verified, overview, backend, trace_label).await;
    }
    Ok(Vec::new())
}

async fn audit_section(
    location: &str,
    evidence: &str,
    section: &AuditSection<'_>,
    backend: &Backend,
    trace_label: &str,
) -> Result<Vec<String>> {
    let numbered = section
        .lines
        .iter()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line_number, line)| format!("L{line_number:04} {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let base = format!(
        "AUDIT LOCATION: {location}\n\nEVIDENCE:\n\n{evidence}\n---\n\n\
         DRAFT EXCERPT TO AUDIT -- ORIGINAL LINE IDS:\n\n{numbered}",
    );
    let label = format!("{trace_label}-audit-{location}");
    let mut correction = String::new();
    for attempt in 1..=2 {
        let prompt = format!("{base}{correction}");
        trace_digest_attempt(&label, attempt, "input.txt", &prompt)?;
        let verdict = backend
            .chat_audit(DAILY_DIGEST_AUDIT_AGENT, &prompt, Some(0.0))
            .await
            .with_context(|| format!("daily digest factual audit failed for {location}"))?;
        trace_digest_attempt(&label, attempt, "output.txt", &verdict)?;
        match parse_audit_verdict(&verdict, location, &section.lines) {
            Ok(issues) => return Ok(issues),
            Err(err) if attempt == 1 => {
                eprintln!("[digestive] factual auditor response rejected: {err:#}");
                correction = format!(
                    "\n\nMANDATORY AUDIT FORMAT CORRECTION:\n\
                     The previous verdict was rejected: {err:#}\n\
                     Audit the same excerpt again. Use location {location} \
                     and copy one `LNNNN` citation ID from DRAFT EXCERPT TO \
                     AUDIT. Do not cite a source email or another topic.\n",
                );
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the bounded audit loop always returns")
}

fn parse_audit_verdict(
    verdict: &str,
    location: &str,
    lines: &[(usize, &str)],
) -> Result<Vec<String>> {
    if verdict.trim() == "OK" {
        return Ok(Vec::new());
    }
    let mut issues = Vec::new();
    for line in verdict.lines().filter(|line| !line.trim().is_empty()) {
        let line = line.trim().trim_start_matches(['-', '*', ' ']).trim();
        let mut fields = line.splitn(3, "|||").map(str::trim);
        let topic = fields.next().unwrap_or_default();
        let citation = fields
            .next()
            .context("factual audit finding has no draft line citation")?;
        let issue = fields
            .next()
            .context("factual audit finding has no explanation")?;
        anyhow::ensure!(
            topic == location,
            "factual audit named {topic}, expected {location}"
        );
        anyhow::ensure!(
            !issue.is_empty(),
            "factual audit finding has an empty explanation"
        );
        let line_number = citation
            .strip_prefix('L')
            .context("factual audit citation does not start with `L`")?
            .parse::<usize>()
            .context("factual audit citation is not a line number")?;
        let (_, line) = lines
            .iter()
            .find(|(number, _)| *number == line_number)
            .with_context(|| {
                format!("factual auditor cited line `{citation}` outside {location}")
            })?;
        anyhow::ensure!(
            !line.trim().is_empty(),
            "factual auditor cited a blank draft line"
        );
        issues.push(format!("{topic}: line {line_number} `{line}` -- {issue}"));
    }
    if issues.is_empty() {
        anyhow::bail!("the factual auditor rejected the draft without naming an issue");
    }
    issues.truncate(MAX_ISSUES);
    Ok(issues)
}

fn output_contract(mode: &DigestMode, ids: &[&str]) -> String {
    format!(
        "FINAL OUTPUT CONTRACT -- CHECKED MECHANICALLY BEFORE PUBLICATION:\n\
         - Length: {}-{} words, excluding internal topic tags. Aim for {} words \
         so there is room for correction. Omit lower-priority topics rather \
         than overrun or strip necessary context.\n\
         - Exactly these level-2 headings, in this order and no others: \
         `## The day in brief`, `## Notable threads`, `## In brief`.\n\
         - Exactly three to six level-3 headings, all between \
         `## Notable threads` and `## In brief`, each naming one thread's \
         actual work and subsystem, and no two alike.\n\
         - Provenance: every level-3 heading begins `### [TNN] ` and every \
         `## In brief` bullet begins `- [TNN] `, where `TNN` is the ID of \
         the one topic packet that section is written from. The only valid \
         IDs are: {}.\n\
         - Each selected topic ID appears only once in the whole digest: a topic \
         is either a notable-thread subsection or an in-brief bullet, never \
         both, and never twice. You are not required to cover every packet. \
         Select only as many topics as fit the word budget with useful context.\n\
         - Never write `no new developments`, `nothing new`, or any other \
         inactivity phrase. A topic you have nothing to say about is left \
         out; it is never described as quiet.\n\
         - Inside `## Notable threads`: prose only. No deeper headings, no \
         numbered lists, and no label headings or bold pseudo-labels such as \
         `Key details`, `Key developments`, `Status`, `Impact`, \
         `What changed` or `Why it matters`.\n\
         - Print no date other than the one in the title, and never print a \
         `Thread root:` marker or a date-key such as `2026/05/22`.\n\
         - Add no statistics, roundup, integration-status or outlook section.",
        mode.min_words,
        mode.max_words,
        (mode.min_words + mode.max_words) / 2,
        ids.join(", "),
    )
}

fn correction_block(issues: &[String]) -> String {
    let issues = issues
        .iter()
        .map(|issue| format!("- {issue}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "MANDATORY CORRECTION -- THE PREVIOUS DRAFT WAS REJECTED:\n{issues}\n\
             Write the digest again from the evidence above, from scratch. The \
             rejected draft is not shown to you and none of its shape may be \
             reused. Satisfy every point of the FINAL OUTPUT CONTRACT above.",
    )
}

/// The deterministic test mock can emit neither extractor syntax nor a
/// contract-conforming digest; pipeline tests exercise plumbing only.
#[cfg(test)]
fn is_mock(backend: &Backend) -> bool {
    matches!(backend, Backend::Mock { .. })
}

#[cfg(not(test))]
fn is_mock(_backend: &Backend) -> bool {
    false
}

/// Split a leading `[TNN]` provenance tag off a heading's or bullet's
/// text, returning the ID and the text that follows it.
fn split_topic_tag(text: &str) -> (Option<&str>, &str) {
    let text = text.trim();
    let (tagged, emphasis) = match text.strip_prefix("**") {
        Some(tagged) => (tagged, "**"),
        None => (text, ""),
    };
    let Some((id, rest)) = tagged.strip_prefix('[').and_then(|t| t.split_once(']')) else {
        return (None, text);
    };
    match id.strip_prefix('T') {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            match rest.strip_prefix(emphasis) {
                Some(rest) => (Some(id), rest.trim_start()),
                None => (None, text),
            }
        }
        _ => (None, text),
    }
}

/// Split a line into its Markdown marker (`###` or `-`, plus the space
/// after it) and the text that follows, for lines that carry one.
fn split_marker(line: &str) -> Option<(&str, &str)> {
    let indent = line.len() - line.trim_start().len();
    let body = &line[indent..];
    let marker = match body.as_bytes().first()? {
        b'#' => body.find(|c| c != '#')?,
        b'-' => 1,
        _ => return None,
    };
    let rest = body[marker..].strip_prefix(' ')?;
    Some((&line[..indent + marker + 1], rest.trim_start()))
}

fn normalize_topic_tags(md: &str) -> String {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"\[T\d+\]\s*").unwrap());
    let mut out = String::with_capacity(md.len());

    for line in md.lines() {
        let keep_leading =
            split_marker(line).is_some_and(|(_, rest)| split_topic_tag(rest).0.is_some());
        if keep_leading {
            let (lead, rest) = split_marker(line).expect("checked above");
            let (tag, text) = split_topic_tag(rest);
            out.push_str(lead);
            out.push('[');
            out.push_str(tag.expect("checked above"));
            out.push_str("] ");
            out.push_str(&tag_re.replace_all(text, ""));
        } else {
            out.push_str(&tag_re.replace_all(line, ""));
        }
        out.push('\n');
    }
    out
}

/// Remove the internal `[TNN]` provenance tags, which exist only so
/// that validation and the factual audit can bind each section to one
/// evidence packet.  Published digests never carry them.
fn strip_topic_tags(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        match split_marker(line).map(|(lead, rest)| (lead, split_topic_tag(rest))) {
            Some((lead, (Some(_), text))) => {
                out.push_str(lead);
                out.push_str(text);
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Remove a final `## In brief` heading that has no items: when every
/// selected topic is notable, nothing is left to put under it.
fn drop_empty_brief_section(md: &str) -> String {
    match md.trim_end().rsplit_once('\n') {
        Some((head, last)) if last.starts_with("## ") && label_key(last) == "in brief" => {
            format!("{}\n", head.trim_end())
        }
        _ => md.to_string(),
    }
}

fn normalize_typography(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for ch in md.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201c}' | '\u{201d}' => out.push('"'),
            '\u{2013}' => out.push('-'),
            '\u{2014}' => out.push_str("--"),
            '\u{2192}' => out.push_str("->"),
            '\u{2026}' => out.push_str("..."),
            '\u{00b2}' => out.push_str("^2"),
            other => out.push(other),
        }
    }
    out
}

fn label_key(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '`' | '#'))
        .collect::<String>()
        .trim()
        .trim_end_matches([':', '.'])
        .trim()
        .to_lowercase()
}

fn is_generic_label(text: &str) -> bool {
    let key = label_key(text);
    GENERIC_LABELS.contains(&key.strip_suffix(" today").unwrap_or(&key))
}

/// Sorted, de-duplicated, comma-joined ID list for a validation message.
fn joined<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let mut list: Vec<&str> = ids.collect();
    list.sort_unstable();
    list.dedup();
    list.join(", ")
}

const INACTIVITY_PHRASES: [&str; 6] = [
    "no new developments",
    "no notable developments",
    "no noteworthy developments",
    "no significant developments",
    "no new activity",
    "nothing new to report",
];

fn validate_digest(md: &str, mode: &DigestMode, ids: &[&str], day: &str) -> Vec<String> {
    static NUMBERED_RE: OnceLock<Regex> = OnceLock::new();
    static DATE_RE: OnceLock<Regex> = OnceLock::new();
    static BOLD_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let numbered_re = NUMBERED_RE.get_or_init(|| Regex::new(r"^ {0,3}\d+[.)]\s").unwrap());
    let date_re = DATE_RE.get_or_init(|| Regex::new(r"\d{4}/\d{2}/\d{2}").unwrap());
    let bold_re = BOLD_RE.get_or_init(|| Regex::new(r"^\*\*([^*]+)\*\*:?").unwrap());
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"\[T\d+\]").unwrap());

    let words = digest_word_count(md);
    let (mut deep, mut outside, mut generic) = (0, 0, 0);
    let (mut numbered, mut roots, mut dated, mut untagged) = (0, 0, 0, 0);
    let (mut h2, mut h3, mut tags): (Vec<String>, Vec<String>, Vec<&str>) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut section = 0;
    let mut fenced = false;
    let mut title_count = 0;
    let mut extra_prose = 0;
    let mut in_brief_item = false;

    for line in md.lines().map(str::trim) {
        if line.is_empty() {
            in_brief_item = false;
            continue;
        }
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }

        let level = line.chars().take_while(|c| *c == '#').count();
        let text = line[level..].trim();
        match level {
            1 => {
                title_count += 1;
                continue;
            }
            2 => {
                in_brief_item = false;
                let key = label_key(text);
                section = match key.as_str() {
                    "notable threads" => 1,
                    "in brief" => 2,
                    _ => section,
                };
                h2.push(key);
            }
            3 => {
                let (tag, named) = split_topic_tag(text);
                match tag {
                    Some(id) => tags.push(id),
                    None => untagged += 1,
                }
                h3.push(label_key(named));
                outside += usize::from(section != 1);
                generic += usize::from(is_generic_label(named));
            }
            0 if section == 1 => {
                numbered += usize::from(numbered_re.is_match(line));
                generic += usize::from(
                    bold_re
                        .captures(line)
                        .is_some_and(|caps| is_generic_label(&caps[1])),
                );
            }
            0 if section == 2 => {
                if let Some(item) = line.strip_prefix("- ") {
                    in_brief_item = true;
                    match split_topic_tag(item).0 {
                        Some(id) => tags.push(id),
                        None => untagged += 1,
                    }
                } else if !in_brief_item {
                    extra_prose += 1;
                }
            }
            0 if h2.is_empty() => extra_prose += 1,
            0 => {}
            _ => deep += 1,
        }
        roots += usize::from(line.to_lowercase().contains("thread root:"));
        dated += usize::from(date_re.is_match(line));
    }

    let mut unique = h3.clone();
    unique.sort();
    unique.dedup();
    let unknown = joined(tags.iter().filter(|id| !ids.contains(*id)).copied());
    let repeated = joined(
        tags.iter()
            .enumerate()
            .filter(|(n, id)| tags[..*n].contains(id))
            .map(|(_, id)| *id),
    );

    let mut issues = Vec::new();
    if !(mode.min_words..=mode.max_words).contains(&words) {
        issues.push(format!(
            "word count is {words}; the {} digest must contain {}-{} words",
            mode.label, mode.min_words, mode.max_words,
        ));
    }
    let title = format!("# Git mailing list daily digest for {day}");
    if title_count != 1 || md.trim().lines().next().map(str::trim) != Some(title.as_str()) {
        issues.push(format!(
            "the first content must be the sole title `{title}`"
        ));
    }
    if extra_prose > 0 {
        issues.push(
            "write only the digest: no preamble, and no unbulleted footer or word-count \
             report under `## In brief`"
                .into(),
        );
    }
    if h2 != DIGEST_H2_SEQUENCE {
        issues.push(format!(
            "there are {} level-2 headings; they must be exactly \
             `## The day in brief`, `## Notable threads` and `## In brief`, \
             in that order and nothing else",
            h2.len(),
        ));
    }
    if !(3..=6).contains(&h3.len()) {
        issues.push(format!(
            "there are {} level-3 headings; there must be three to six",
            h3.len(),
        ));
    }
    if untagged > 0 {
        issues.push(format!(
            "{untagged} level-3 heading(s) or `## In brief` bullet(s) do not \
             start with the `[TNN]` ID of the topic packet they are written \
             from",
        ));
    }
    if !unknown.is_empty() {
        issues.push(format!(
            "topic ID(s) {unknown} are not in the evidence; the only valid \
             IDs are {}",
            ids.join(", "),
        ));
    }
    if !repeated.is_empty() {
        issues.push(format!(
            "topic ID(s) {repeated} appear more than once; each topic gets \
             exactly one subsection or one bullet, never both",
        ));
    }
    let lower = md.to_lowercase();
    if let Some(phrase) = INACTIVITY_PHRASES
        .iter()
        .find(|phrase| lower.contains(**phrase))
    {
        issues.push(format!(
            "the draft writes `{phrase}`; a topic with nothing worth \
             reporting is left out, never reported as quiet",
        ));
    }
    for (count, message) in [
        (
            outside,
            "level-3 heading(s) sit outside `## Notable threads`",
        ),
        (
            h3.len() - unique.len(),
            "level-3 heading(s) repeat an earlier heading",
        ),
        (
            deep,
            "heading(s) are deeper than level 3; write prose instead",
        ),
        (
            generic,
            "generic label heading(s) such as `Status` or `Key developments`; \
             name the actual work and subsystem instead",
        ),
        (
            numbered,
            "numbered list item(s) inside `## Notable threads`; write prose there",
        ),
        (
            roots,
            "line(s) print a `Thread root:` marker; that is input bookkeeping, \
             never digest text",
        ),
        (
            dated,
            "line(s) outside the title print a `YYYY/MM/DD` date; a thread \
             root's date is not the date of today's development",
        ),
        (
            // Whatever survives stripping is a tag the publication step
            // cannot remove.
            tag_re.find_iter(&strip_topic_tags(md)).count(),
            "stray `[TNN]` topic ID(s) sit outside a level-3 heading's or \
             an in-brief bullet's leading position; the ID belongs there \
             and nowhere else",
        ),
    ] {
        if count > 0 {
            issues.push(format!("{count} {message}"));
        }
    }
    issues.truncate(MAX_ISSUES);
    issues
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
         Extract today's developments from the source emails below. \
         Quoted passages identify earlier proposals and referents; they \
         are not new contributions by the current message's author.\n\n",
    );

    for activity in threads {
        if !activity
            .email_summaries
            .iter()
            .any(|email| email.source_excerpt.is_some())
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

        if let Some(background) = &activity.root_source_excerpt {
            user_msg.push_str("THREAD BACKGROUND -- TOPIC SOURCE, NOT TODAY'S DEVELOPMENTS:\n\n");
            user_msg.push_str(background);
            user_msg.push_str("\n\n");
        }

        user_msg.push_str(
            "TODAY'S SOURCE EMAILS -- COMPLETE TEXT, INCLUDING QUOTED \
             CONTEXT. ATTRIBUTE ONLY UNQUOTED CONTRIBUTIONS TO THE \
             CURRENT AUTHOR:\n\n",
        );
        for email in &activity.email_summaries {
            let Some(source) = &email.source_excerpt else {
                continue;
            };
            match &email.from {
                Some(f) => user_msg.push_str(&format!("[{} by {f}]\n", email.dk)),
                None => user_msg.push_str(&format!("[{}]\n", email.dk)),
            }
            user_msg.push_str(source);
            user_msg.push_str("\n\n");
        }
    }

    user_msg.push_str(
        "---\n\n\
         FINAL MANDATORY CHECK BEFORE WRITING:\n\
         Every development must be stated in today's unquoted source text. \
         Distinguish a new revision's changelog from unchanged features. \
         Keep later corrections and retractions, and preserve the current \
         subject when it differs from the older thread root's title.\n",
    );

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
        if path.contains(".thread.")
            || path.ends_with("/digest.ai.md")
            || path.ends_with("/digest.weekly.ai.md")
        {
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
    fn test_build_day_digest_loads_root_source_evidence() {
        let root_md = format!(
            "# [PATCH v2 0/3] pack-objects: teach --stdin-packs a fast path\n\n\
             | Header | Value |\n|---|---|\n| **From** | Patrick Steinhardt <ps@pks.im> |\n\n\
             ---\n\n**Thread**: [t](t.md)\n\n\
             On Monday, a reviewer wrote:\n\
             > every reroll must stop relying on the_repository\n\n\
             This series makes `git pack-objects --stdin-packs` avoid the \
             quadratic lookup over the packfile list.\n{}",
            "Padding sentence that pushes the root email past the bound. ".repeat(60),
        );
        let mut blobs = MockBlobs(Default::default());
        blobs
            .0
            .insert("test:2026/08/13/05-47-56.md".into(), root_md);
        let summaries = vec![(
            "2026/08/13/17-35-39".into(),
            "2026/08/13/05-47-56".into(),
            "reply ai".into(),
        )];
        let (threads, _) = build_day_digest_input(&summaries, "before", "test", &mut blobs);

        let evidence = threads[0]
            .root_source_excerpt
            .as_deref()
            .expect("thread root source must be loaded as topic evidence");
        assert!(
            evidence.starts_with(
                "Subject: [PATCH v2 0/3] pack-objects: teach --stdin-packs a fast path\n\n"
            ),
            "evidence must lead with the root subject: {evidence}",
        );
        assert!(
            evidence.contains("avoid the quadratic lookup"),
            "root author's own words must survive: {evidence}",
        );
        assert!(
            !evidence.contains("stop relying on the_repository"),
            "quoted text is not the root author's contribution: {evidence}",
        );
        assert!(
            evidence.contains("[... source email truncated ...]"),
            "an oversized root email must be truncated, not passed whole: {evidence}",
        );
        assert!(
            evidence.chars().count() <= 2200,
            "evidence must stay bounded, got {} chars",
            evidence.chars().count(),
        );
    }

    #[test]
    fn test_topic_context_uses_latest_cover_without_future_leakage() {
        let root = "2026/06/14/19-25-39";
        let cover = "2026/08/07/07-39-23";
        let reply = "2026/08/13/21-52-53";
        let mut tree = ThreadTree::new();
        let mut blobs = MockBlobs(Default::default());
        for (dk, subject, body) in [
            (
                root,
                "[PATCH 0/8] rebase: add --fixup",
                "Original rebase proposal.",
            ),
            (
                cover,
                "[PATCH v13 0/8] history: add squash subcommand",
                "Current history squash proposal.",
            ),
            (
                "2026/08/12/12-00-00",
                "[PATCH v13 8/8] completion: add history support",
                "Individual patch, not series context.",
            ),
            (
                reply,
                "Re: [PATCH v13 7/8] history: create squashed commits",
                "Queued a test fix.",
            ),
            (
                "2026/08/14/12-00-00",
                "[PATCH v14 0/8] future: rename the command",
                "Future context must not leak.",
            ),
        ] {
            tree.insert(dk, (dk != root).then_some(root), subject, "Author");
            blobs.0.insert(
                format!("test:{dk}.md"),
                format!("# {subject}\n\n**Thread**: [thread](t.md)\n\n{body}"),
            );
        }
        blobs
            .0
            .insert(format!("test:{root}.thread.md"), tree.render(root));
        let summaries = vec![(
            reply.into(),
            root.into(),
            "Untrusted generated summary.".into(),
        )];
        let (threads, _) = build_day_digest_input(&summaries, "before", "test", &mut blobs);
        let context = threads[0].root_source_excerpt.as_ref().unwrap();
        assert!(
            context.contains("Current history squash proposal."),
            "{context}"
        );
        assert!(!context.contains("Original rebase proposal."));
        assert!(!context.contains("Individual patch"));
        assert!(!context.contains("Future context"));
        let earlier =
            topic_source_evidence(root, "2026/07/01/00-00-00", "test", &mut blobs).unwrap();
        assert!(earlier.contains("Original rebase proposal."));
        blobs.0.remove(&format!("test:{cover}.md"));
        assert!(topic_source_evidence(root, reply, "test", &mut blobs).is_none());
    }

    #[test]
    fn test_topic_posts_exclude_replies_and_individual_series_patches() {
        for subject in [
            "[PATCH RFC v3 00/13] Topic",
            "[GSoC PATCH v5 0/6] Topic",
            "[PATCH v2] Standalone patch",
            "[PATCH/RFC] Standalone proposal",
        ] {
            assert!(is_topic_post(subject), "{subject}");
        }
        for subject in [
            "Re: [PATCH v3 0/6] Review",
            "[PATCH v13 7/8] One series member",
            "[PATCH v2 0/0] Malformed count",
            "[ANNOUNCE] Release",
        ] {
            assert!(!is_topic_post(subject), "{subject}");
        }
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
            root_source_excerpt: None,
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/07/01/08-50-41".into(),
                ai: "poisoned candidate rationale".into(),
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
            !msg.contains("prior summary claiming merged to master"),
            "generated thread state must not reach source extraction",
        );
        assert!(
            msg.contains("[2026/07/01/08-50-41 by Toon Claes]\nThe source reply."),
            "each authored excerpt should follow a `by <Name>` header, got:\n{msg}",
        );
        assert!(
            !msg.contains("poisoned candidate rationale"),
            "the per-email AI brief must never reach the extractor: {msg}",
        );
        assert!(
            !msg.contains("EXCLUSION BASELINE"),
            "extraction must not depend on generated baselines",
        );
        assert!(
            msg.contains("TODAY'S SOURCE EMAILS -- COMPLETE TEXT"),
            "today's contributions must retain their source context",
        );
        assert!(
            msg.contains("FINAL MANDATORY CHECK BEFORE WRITING"),
            "missing recency-positioned differential check",
        );
    }

    #[test]
    fn test_delta_inputs_exclude_ai_briefs_but_keep_authored_source() {
        let activity = |source_is_short| ThreadDayActivity {
            root_dk: "2026/05/01/21-35-37".into(),
            thread_ai_before: Some("poisoned prior rationale".into()),
            root_source_excerpt: None,
            email_summaries: vec![DayEmailEvidence {
                dk: "2026/08/07/13-09-14".into(),
                ai: "poisoned candidate rationale".into(),
                from: Some("Phillip Wood".into()),
                source_excerpt: Some(
                    "Subject: Re: [PATCH v25 0/7] branch: delete-merged\n\n\
                     It looks ready to me\n\nThanks\n\nPhillip"
                        .into(),
                ),
                source_is_short,
            }],
            wc_status: None,
        };

        for source_is_short in [false, true] {
            let input = build_daily_digest_user_msg("2026/08/07", &[activity(source_is_short)], 1);
            assert!(input.contains("It looks ready to me"));
            assert!(!input.contains("poisoned prior rationale"));
            assert!(!input.contains("poisoned candidate rationale"));
        }
    }

    #[test]
    fn test_email_evidence_preserves_complete_source() {
        let body = format!(
            "> Earlier proposal: first move the sources into lib/.\n\n\
             Initially I agreed.\n{}\n\
             After considering it, I favor smaller conceptual groups instead.\n\
             Changes since v2: preserve the new conclusion and rationale.\n{}\n",
            "Discussion leading to a different conclusion. ".repeat(30),
            "Supporting example. ".repeat(30),
        );
        let mut blobs = MockBlobs(Default::default());
        blobs.0.insert(
            "source:2026/08/13/19-04-52.md".into(),
            format!("# Layout discussion\n\n**Thread**: [thread](thread.md)\n\n{body}"),
        );
        let (_, source, _) = email_evidence("2026/08/13/19-04-52", "source", &mut blobs);
        assert_eq!(
            source.unwrap(),
            format!("Subject: Layout discussion\n\n{body}")
        );
    }

    #[tokio::test]
    async fn test_extract_thread_deltas_isolates_complete_source() {
        use wiremock::matchers::{body_string_contains, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut threads = [
            delta_activity("2026/08/12/00-00-00", &[("2026/08/13/09-00-00", "Alice")]),
            delta_activity("2026/08/12/01-00-00", &[("2026/08/13/10-00-00", "Bob")]),
        ];
        for (index, thread) in threads.iter_mut().enumerate() {
            thread.email_summaries[0].source_is_short = index == 1;
            thread.email_summaries[0].source_excerpt = Some(format!(
                "COMPLETE-SOURCE-{index}\n{}",
                "source ".repeat(100)
            ));
            Mock::given(method("POST"))
                .and(body_string_contains(format!("Thread root: {}", thread.root_dk)))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "role": "assistant",
                        "content": format!("- [{}] Posted a correction.", thread.email_summaries[0].dk)
                    }}]
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        for thread in &threads {
            let deltas = extract_thread_deltas("2026/08/13", thread, &backend)
                .await
                .unwrap();
            assert_eq!(deltas.len(), 1);
            assert_eq!(deltas[0].root_dk, thread.root_dk);
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let input = body["messages"][1]["content"].as_str().unwrap();
            assert!(
                input.contains(
                    threads[index].email_summaries[0]
                        .source_excerpt
                        .as_ref()
                        .unwrap()
                )
            );
            assert!(!input.contains(&format!("COMPLETE-SOURCE-{}", 1 - index)));
            assert!(!input.contains("A generated baseline"));
            assert!(!input.contains("Untrusted brief."));
        }
        threads[0].email_summaries[0].source_excerpt = None;
        assert!(
            extract_thread_deltas("2026/08/13", &threads[0], &backend)
                .await
                .unwrap_err()
                .to_string()
                .contains("missing source email")
        );
    }

    #[tokio::test]
    async fn test_extract_thread_deltas_rejects_cross_thread_citations() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "- [2026/08/13/10-00-00] Unrelated."
                }}]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        let thread = delta_activity("2026/08/12/00-00-00", &[("2026/08/13/09-00-00", "Alice")]);
        let err = extract_thread_deltas("2026/08/13", &thread, &backend)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown source email"));
    }

    fn thread_activity(root_dk: &str, source: Option<&str>) -> ThreadDayActivity {
        ThreadDayActivity {
            root_dk: root_dk.into(),
            thread_ai_before: Some(
                "A generated baseline that must never reach the final editor.".into(),
            ),
            root_source_excerpt: source.map(str::to_string),
            email_summaries: Vec::new(),
            wc_status: None,
        }
    }

    fn delta_activity(root_dk: &str, emails: &[(&str, &str)]) -> ThreadDayActivity {
        let mut activity = thread_activity(root_dk, None);
        activity.email_summaries = emails
            .iter()
            .map(|(dk, author)| DayEmailEvidence {
                dk: (*dk).into(),
                ai: "Untrusted brief.".into(),
                from: Some((*author).into()),
                source_excerpt: Some("Authored source.".into()),
                source_is_short: false,
            })
            .collect();
        activity
    }

    #[test]
    fn test_parse_root_deltas_uses_message_provenance() {
        let threads = [
            delta_activity(
                "2026/07/15/01-18-50",
                &[("2026/08/13/06-11-25", "Tian Yuchen")],
            ),
            delta_activity(
                "2026/07/16/13-28-41",
                &[("2026/08/13/20-08-24", "Siddharth Shrimali")],
            ),
        ];
        let raw = "Thread root: 2026/07/15/01-18-50\n\
                   New today:\n\
                   - [2026/08/13/20-08-24 by Wrong Author] Posted v5.";
        assert_eq!(
            parse_root_deltas(raw, &threads).unwrap(),
            vec![RootDelta {
                root_dk: "2026/07/16/13-28-41".into(),
                bullets: vec!["[2026/08/13/20-08-24 by Siddharth Shrimali] Posted v5.".into()],
            }],
        );
    }

    #[test]
    fn test_parse_root_deltas_orders_interleaved_replies() {
        let threads = [delta_activity(
            "2026/08/06/06-40-13",
            &[
                ("2026/08/13/17-00-40", "Mark Levedahl"),
                ("2026/08/13/19-35-46", "Johannes Sixt"),
                ("2026/08/13/20-11-08", "Mark Levedahl"),
            ],
        )];
        let raw = "- [2026/08/13/17-00-40] Described the color incorrectly.\n\
                   - [2026/08/13/20-11-08] Retracted that description.\n\
                   - [2026/08/13/20-11-08] Suggested a clearer label.\n\
                   No noteworthy thread deltas.\n\
                   - [2026/08/13/19-35-46] Corrected the description.";
        assert_eq!(
            parse_root_deltas(raw, &threads).unwrap()[0].bullets,
            vec![
                "[2026/08/13/17-00-40 by Mark Levedahl] Described the color incorrectly.",
                "[2026/08/13/19-35-46 by Johannes Sixt] Corrected the description.",
                "[2026/08/13/20-11-08 by Mark Levedahl] Retracted that description.",
                "[2026/08/13/20-11-08 by Mark Levedahl] Suggested a clearer label.",
            ],
        );
    }

    #[test]
    fn test_parse_root_deltas_keeps_nested_details_with_their_source() {
        let threads = [delta_activity(
            "2026/07/08/02-59-40",
            &[("2026/08/13/01-05-33", "Michael Montalbo")],
        )];
        let raw = "- [2026/08/13/01-05-33] Posted v3:\n  \
                   - Use rm without -f for atomic consumption.\n  \
                   - Move documentation to t/lib-httpd.sh.";
        assert_eq!(
            parse_root_deltas(raw, &threads).unwrap()[0].bullets,
            vec![
                "[2026/08/13/01-05-33 by Michael Montalbo] Posted v3: \
                 - Use rm without -f for atomic consumption. \
                 - Move documentation to t/lib-httpd.sh.",
            ],
        );
        assert!(
            parse_root_deltas(
                "- [2026/08/13/01-05-33] Posted v3.\n- An uncited development.",
                &threads,
            )
            .is_err()
        );
    }

    /// The two extractors' outputs arrive concatenated, optionally
    /// fenced, and may both report the same root; the parser has to
    /// recover exactly one record per root, in thread order.
    #[test]
    fn test_parse_root_deltas_merges_extractor_outputs() {
        let threads = [
            delta_activity(
                "2026/08/12/00-00-00",
                &[("2026/08/13/10-00-00", "Jeff King")],
            ),
            delta_activity(
                "2026/08/13/05-47-56",
                &[
                    ("2026/08/13/09-00-00", "Patrick Steinhardt"),
                    ("2026/08/13/17-35-39", "Junio C Hamano"),
                ],
            ),
        ];
        let raw = "```markdown\n\
                   ---\n\
                   Thread root: 2026/08/13/05-47-56\n\
                   New today:\n\
                   - [2026/08/13/09-00-00 by Patrick Steinhardt] Posted v3, which\n\
                   drops the extra allocation.\n\
                   ```\n\n\
                   ---\n\
                   Thread root: 2026/08/12/00-00-00\n\
                   New today:\n\
                   - [2026/08/13/10-00-00 by Jeff King] Reported a leak in `revision.c`.\n\
                   ---\n\
                   Thread root: 2026/08/13/05-47-56\n\
                   New today:\n\
                   - [2026/08/13/17-35-39 by Junio C Hamano] Said it looked good.\n\
                   No noteworthy short-reply deltas.\n";

        assert_eq!(
            parse_root_deltas(raw, &threads).unwrap(),
            vec![
                RootDelta {
                    root_dk: "2026/08/12/00-00-00".into(),
                    bullets: vec![
                        "[2026/08/13/10-00-00 by Jeff King] Reported a leak in `revision.c`."
                            .into(),
                    ],
                },
                RootDelta {
                    root_dk: "2026/08/13/05-47-56".into(),
                    bullets: vec![
                        "[2026/08/13/09-00-00 by Patrick Steinhardt] Posted v3, which \
                         drops the extra allocation."
                            .into(),
                        "[2026/08/13/17-35-39 by Junio C Hamano] Said it looked good.".into(),
                    ],
                },
            ],
        );
    }

    /// The extractor sometimes writes every bullet as `[date-key] text`,
    /// without the leading dash.
    #[test]
    fn test_parse_root_deltas_accepts_bullets_without_marker() {
        let threads = [delta_activity(
            "2026/08/13/05-47-56",
            &[
                ("2026/08/13/05-47-56", "Patrick Steinhardt"),
                ("2026/08/13/17-35-39", "Junio C Hamano"),
            ],
        )];
        let raw = "[2026/08/13/05-47-56] Posted\n\
                   [PATCH v2 0/5] with the rebased series.\n\
                   [2026/08/13/17-35-39 by Wrong Author] Agreed.";
        assert_eq!(
            parse_root_deltas(raw, &threads).unwrap()[0].bullets,
            vec![
                "[2026/08/13/05-47-56 by Patrick Steinhardt] Posted \
                 [PATCH v2 0/5] with the rebased series.",
                "[2026/08/13/17-35-39 by Junio C Hamano] Agreed.",
            ],
        );
    }

    #[test]
    fn test_parse_root_deltas_rejects_malformed_output() {
        let threads = [delta_activity(
            "2026/08/13/05-47-56",
            &[("2026/08/13/17-35-39", "Junio C Hamano")],
        )];

        assert!(
            parse_root_deltas("No noteworthy thread deltas.", &threads)
                .unwrap()
                .is_empty(),
            "the sentinel is not malformed; it simply carries no delta",
        );
        assert!(
            parse_root_deltas(
                "Here are the deltas for threads with new information today:\n\
                 Thread root: 2026/08/13/05-47-56\n\
                 New today:\n\
                 - [2026/08/13/17-35-39 by Junio C Hamano] Agreed.\n\
                 None.",
                &threads,
            )
            .is_ok(),
            "a descriptive extractor preamble is harmless",
        );

        for (raw, expected) in [
            (
                "---\nThread root: 2026/01/01/00-00-00\nNew today:\n- Something.",
                "unknown thread root",
            ),
            ("Today the list discussed several series.", "stray line"),
            ("[2026/08/12/00-00-00] Posted v5.", "stray line"),
            ("- Something.", "without an email citation"),
            ("- [2026/08/12/00-00-00] Posted v5.", "unknown source email"),
            ("- [2026/08/13/17-35-39]", "empty delta"),
        ] {
            let err = parse_root_deltas(raw, &threads).unwrap_err().to_string();
            assert!(err.contains(expected), "expected `{expected}`, got `{err}`");
        }
        assert_eq!(
            parse_root_deltas("- [2026/08/13/17-35-39] Agreed.", &threads).unwrap()[0].bullets,
            vec!["[2026/08/13/17-35-39 by Junio C Hamano] Agreed."],
        );
    }

    #[test]
    fn test_topic_packets_colocate_background_with_own_deltas() {
        let threads = [
            thread_activity(
                "2026/08/13/05-47-56",
                Some(
                    "Subject: [PATCH v2 0/3] odb: drop the_repository from lookups\n\n\
                     This series changes object database lookup so that it no \
                     longer depends on repository-global state.",
                ),
            ),
            thread_activity(
                "2026/08/12/00-00-00",
                Some("Subject: Old topic\n\nUNRELATED BACKGROUND."),
            ),
            thread_activity("2026/08/11/00-00-00", None),
        ];
        let deltas = vec![
            RootDelta {
                root_dk: "2026/08/13/05-47-56".into(),
                bullets: vec!["[2026/08/13/17-35-39 by Junio C Hamano] Agreed.".into()],
            },
            // A root the extractor named but left without any bullet.
            RootDelta {
                root_dk: "2026/08/12/00-00-00".into(),
                bullets: Vec::new(),
            },
            RootDelta {
                root_dk: "2026/08/11/00-00-00".into(),
                bullets: vec!["[2026/08/13/10-00-00 by Jeff King] Found a leak.".into()],
            },
        ];

        let packets = build_topic_packets(DIGEST_DAY, &threads, &deltas);
        assert_eq!(packets.len(), 2, "a root without deltas gets no packet");
        assert_eq!(packets[0].id, "T01");
        assert_eq!(packets[1].id, "T02");
        assert!(
            packets[1].background.is_none(),
            "an unreadable thread root yields no background, never a substitute",
        );

        let rendered = render_topic_packets(&packets);
        let second = rendered.find("TOPIC T02").expect("T02 packet");
        assert!(
            rendered[..second].contains("no longer depends on repository-global state")
                && rendered[..second].contains("Agreed."),
            "T01's background and deltas must sit together: {rendered}",
        );
        assert!(
            rendered[second..].contains("NO BACKGROUND FOR T02")
                && rendered[second..].contains("Found a leak."),
            "T02 must carry its own deltas and nothing else: {rendered}",
        );
        assert!(
            !rendered.contains("UNRELATED BACKGROUND"),
            "a root without deltas must not smuggle in background: {rendered}",
        );
        assert!(
            !rendered.contains("2026/08/13/05-47-56") && !rendered.contains("must never reach"),
            "roots are named by opaque ID only, never by date-key or baseline: {rendered}",
        );
    }

    #[test]
    fn test_topic_packet_retains_current_subject_after_rename() {
        let mut thread = delta_activity(
            "2026/06/14/19-25-39",
            &[("2026/08/13/21-52-53", "Junio C Hamano")],
        );
        thread.root_source_excerpt =
            Some("Subject: rebase: add --fixup\n\nOriginal proposal.".into());
        thread.email_summaries[0].source_excerpt = Some(
            "Subject: Re: [PATCH v13 7/8] history: create squashed commits without editing\n\n\
             Queued a test fix."
                .into(),
        );
        let deltas = vec![RootDelta {
            root_dk: thread.root_dk.clone(),
            bullets: vec!["[2026/08/13/21-52-53 by Junio C Hamano] Queued a test fix.".into()],
        }];
        let packets = build_topic_packets(DIGEST_DAY, &[thread], &deltas);
        assert_eq!(
            packets[0].current_subject.as_deref(),
            Some("Re: [PATCH v13 7/8] history: create squashed commits without editing"),
        );
        let rendered = render_topic_packets(&packets);
        assert!(rendered.contains("CURRENT SOURCE SUBJECT FOR T01: Re: [PATCH v13 7/8] history:"));
        assert!(rendered.contains("Subject: rebase: add --fixup"));
    }

    /// The topic IDs the fixture digest below is bound to.
    const VALID_IDS: [&str; 4] = ["T01", "T02", "T03", "T04"];
    const DIGEST_DAY: &str = "2026/08/13";

    fn selection_packets(count: usize) -> Vec<TopicPacket> {
        (1..=count)
            .map(|index| TopicPacket {
                id: format!("T{index:02}"),
                current_subject: Some(format!("Subject {index}")),
                background: None,
                bullets: vec![format!("New development {index}.")],
                source: format!("Source {index}."),
            })
            .collect()
    }

    #[test]
    fn test_topic_selection_bounds_ranked_evidence() {
        let packets = selection_packets(20);
        let ranking: Vec<_> = packets
            .iter()
            .rev()
            .map(|packet| packet.id.clone())
            .collect();
        let raw = format!("```json\n{}\n```", serde_json::to_string(&ranking).unwrap());
        let parsed = parse_topic_ranking(&raw, &packets).unwrap();
        let human = TopicSelection::from_ranking(parsed.clone(), &HUMAN_DIGEST_MODE);
        assert_eq!(human.notable, ranking[..3]);
        assert_eq!(human.brief, ranking[3..11]);
        let ai = TopicSelection::from_ranking(parsed, &AI_DIGEST_MODE);
        assert_eq!(ai.notable, ranking[..5]);
        assert_eq!(ai.brief, ranking[5..17]);
        for invalid in [
            "[]",
            "{}",
            "[\"T01\", \"T02\"]",
            "[\"T01\", \"T02\", \"T01\"]",
            "[\"T01\", \"T02\", \"T99\"]",
            "Here is the ranking: [\"T01\",\"T02\",\"T03\"]",
        ] {
            assert!(parse_topic_ranking(invalid, &packets).is_err(), "{invalid}");
        }
    }

    #[test]
    fn test_selected_notable_roles_are_enforced() {
        let selection = TopicSelection {
            notable: vec!["T01".into(), "T02".into(), "T03".into()],
            brief: vec!["T04".into()],
        };
        let draft = valid_digest(&HUMAN_DIGEST_MODE, 0);
        assert!(selection_issue(&draft, &selection).is_none());
        let swapped = draft
            .replace("### [T02]", "### [T04]")
            .replace("- [T04]", "- [T02]");
        assert!(validate_digest(&swapped, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY).is_empty());
        assert!(selection_issue(&swapped, &selection).is_some());
    }

    #[tokio::test]
    async fn test_selection_does_not_request_impossible_topic_counts() {
        let err = select_digest_topics(
            DIGEST_DAY,
            &HUMAN_DIGEST_MODE,
            &selection_packets(2),
            &Backend::MockNoChoices,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("at least three source-grounded topics")
        );
    }

    #[tokio::test]
    async fn test_editor_receives_only_bounded_selected_packets() {
        use wiremock::matchers::{body_string_contains, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let packets = selection_packets(20);
        let ranking: Vec<_> = packets
            .iter()
            .rev()
            .map(|packet| packet.id.as_str())
            .collect();
        let ranking = serde_json::to_string(&ranking).unwrap();
        let server = MockServer::start().await;
        for (system, response, calls) in [
            (DAILY_SELECT_AGENT, ranking.as_str(), 1),
            (DAILY_DIGEST_AGENT, "not a digest", 4),
        ] {
            Mock::given(method("POST"))
                .and(body_string_contains(system.lines().next().unwrap()))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": response}}]
                })))
                .expect(calls)
                .mount(&server)
                .await;
        }
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        assert!(
            generate_digest_mode(DIGEST_DAY, &HUMAN_DIGEST_MODE, &packets, &backend)
                .await
                .is_err()
        );
        let requests = server.received_requests().await.unwrap();
        let editor_inputs: Vec<_> = requests
            .iter()
            .filter_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if body["messages"][0]["content"].as_str() == Some(DAILY_DIGEST_AGENT) {
                    Some(body["messages"][1]["content"].as_str().unwrap().to_string())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(editor_inputs.len(), 4);
        for input in editor_inputs {
            assert!(input.contains("TOPIC T20\n"));
            assert!(input.contains("TOPIC T10\n"));
            assert!(!input.contains("TOPIC T09\n"));
            assert!(!input.contains("TOPIC T01\n"));
        }
    }

    #[tokio::test]
    async fn test_revision_does_not_reintroduce_obsolete_findings() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let clean = valid_digest(&HUMAN_DIGEST_MODE, 0);
        let expected = strip_topic_tags(&clean);
        let initial = clean.replace(
            "Prose about the packfile lookup fast path.",
            "Prose about the packfile lookup fast path. OBSOLETE_FACT_A",
        );
        let calls = AtomicUsize::new(0);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let system = body["messages"][0]["content"].as_str().unwrap();
                let user = body["messages"][1]["content"].as_str().unwrap();
                let content = if system == DAILY_SELECT_AGENT {
                    r#"["T01","T02","T03","T04"]"#.to_string()
                } else if system == DAILY_DIGEST_AGENT {
                    initial.clone()
                } else if system == DAILY_REVISE_AGENT {
                    let body = if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                        "Prose about the packfile lookup fast path. CURRENT_FACT_B"
                    } else {
                        "Prose about the packfile lookup fast path."
                    };
                    serde_json::json!({"title": "packfile lookup fast path", "body": body})
                        .to_string()
                } else {
                    assert_eq!(system, DAILY_DIGEST_AUDIT_AGENT);
                    let finding = user.lines().find(|line| {
                        line.starts_with('L')
                            && (line.contains("OBSOLETE_FACT_A") || line.contains("CURRENT_FACT_B"))
                    });
                    match finding {
                        Some(line) => {
                            let location = user
                                .lines()
                                .find_map(|line| line.strip_prefix("AUDIT LOCATION: "))
                                .unwrap();
                            let citation = line.split_whitespace().next().unwrap();
                            let marker = if line.contains("OBSOLETE_FACT_A") {
                                "OBSOLETE_FACT_A"
                            } else {
                                "CURRENT_FACT_B"
                            };
                            format!("- {location} ||| {citation} ||| Remove {marker}")
                        }
                        None => "OK".into(),
                    }
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": content}}]
                }))
            })
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        let result = generate_digest_mode(
            DIGEST_DAY,
            &HUMAN_DIGEST_MODE,
            &selection_packets(4),
            &backend,
        )
        .await
        .unwrap();
        assert_eq!(result, expected);
        let requests = server.received_requests().await.unwrap();
        let editor_inputs: Vec<_> = requests
            .iter()
            .filter_map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                (body["messages"][0]["content"].as_str() == Some(DAILY_REVISE_AGENT))
                    .then(|| body["messages"][1]["content"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(editor_inputs.len(), 2);
        assert!(editor_inputs[0].contains("OBSOLETE_FACT_A"));
        assert!(!editor_inputs[1].contains("OBSOLETE_FACT_A"));
        assert!(editor_inputs[1].contains("CURRENT_FACT_B"));
        for input in editor_inputs {
            assert!(input.contains("Source 1."));
            assert!(!input.contains("Source 2."));
        }
    }

    #[tokio::test]
    async fn test_structural_rejections_do_not_use_up_the_audit_retries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let clean = valid_digest(&HUMAN_DIGEST_MODE, 0);
        let too_long = valid_digest(&HUMAN_DIGEST_MODE, HUMAN_DIGEST_MODE.max_words + 1);
        let flawed = clean.replace(
            "Prose about the packfile lookup fast path.",
            "Prose about the packfile lookup fast path. FLAW",
        );
        let drafts = AtomicUsize::new(0);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let system = body["messages"][0]["content"].as_str().unwrap();
                let user = body["messages"][1]["content"].as_str().unwrap();
                let content = if system == DAILY_SELECT_AGENT {
                    r#"["T01","T02","T03","T04"]"#.to_string()
                } else if system == DAILY_DIGEST_AGENT {
                    if drafts.fetch_add(1, Ordering::Relaxed) < MAX_STRUCTURAL_REJECTIONS - 1 {
                        too_long.clone()
                    } else {
                        flawed.clone()
                    }
                } else if system == DAILY_REVISE_AGENT {
                    serde_json::json!({
                        "title": "packfile lookup fast path",
                        "body": "Prose about the packfile lookup fast path.",
                    })
                    .to_string()
                } else {
                    assert_eq!(system, DAILY_DIGEST_AUDIT_AGENT);
                    match user
                        .lines()
                        .find(|line| line.starts_with('L') && line.contains("FLAW"))
                    {
                        Some(line) => {
                            let location = user
                                .lines()
                                .find_map(|line| line.strip_prefix("AUDIT LOCATION: "))
                                .unwrap();
                            let citation = line.split_whitespace().next().unwrap();
                            format!("- {location} ||| {citation} ||| Remove FLAW")
                        }
                        None => "OK".into(),
                    }
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": content}}]
                }))
            })
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };

        let result = generate_digest_mode(
            DIGEST_DAY,
            &HUMAN_DIGEST_MODE,
            &selection_packets(4),
            &backend,
        )
        .await
        .unwrap();

        assert_eq!(result, strip_topic_tags(&clean));
    }

    #[test]
    fn test_an_empty_in_brief_section_is_dropped() {
        let empty = "# Title\n\n## Notable threads\n\n### [T01] Work\n\nProse.\n\n## In brief\n\n";
        assert_eq!(
            drop_empty_brief_section(empty),
            "# Title\n\n## Notable threads\n\n### [T01] Work\n\nProse.\n"
        );
        let filled = "# Title\n\n## In brief\n\n- Item.\n";
        assert_eq!(drop_empty_brief_section(filled), filled);
    }

    #[tokio::test]
    async fn test_a_digest_without_in_brief_topics_is_published_without_the_heading() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let digest = valid_digest(&HUMAN_DIGEST_MODE, 0)
            .replace("- [T04] One remaining thread saw a small test fix.\n", "");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let system = body["messages"][0]["content"].as_str().unwrap();
                let content = if system == DAILY_SELECT_AGENT {
                    r#"["T01","T02","T03"]"#.to_string()
                } else if system == DAILY_DIGEST_AGENT {
                    digest.clone()
                } else {
                    "OK".to_string()
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": content}}]
                }))
            })
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };

        let result = generate_digest_mode(
            DIGEST_DAY,
            &HUMAN_DIGEST_MODE,
            &selection_packets(3),
            &backend,
        )
        .await
        .unwrap();

        assert!(!result.contains("## In brief"));
    }

    #[tokio::test]
    async fn test_audits_use_the_audit_reasoning_effort() {
        use crate::ai_backend::RequestOptions;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "OK"}}]
            })))
            .mount(&server)
            .await;
        let options = RequestOptions {
            reasoning_effort: Some("low".into()),
            audit_reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        let backend = Backend::azure_openai(server.uri(), None, "key".into(), options);
        let digest = valid_digest(&HUMAN_DIGEST_MODE, 0);

        let issues = audit_digest(
            &digest,
            &selection_packets(4),
            &mut HashMap::new(),
            &backend,
            "test",
        )
        .await
        .unwrap();

        assert!(issues.is_empty());
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        for request in requests {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["reasoning_effort"], "high");
        }
    }

    #[test]
    fn test_revised_items_are_rendered_with_owned_structure() {
        let draft = "## The day in brief\nOverview.\n## Notable threads\n\
                     ### [T01] Old title\nOld text.\n## In brief\n- [T02] Old brief.\n";
        let sections = audit_sections(draft);
        let raw = r#"{"title":"Current topic","body":"Source-backed text."}"#;
        assert_eq!(
            render_revised_item(&sections[0], raw).unwrap(),
            "## The day in brief\n\nSource-backed text.\n\n"
        );
        assert_eq!(
            render_revised_item(&sections[1], raw).unwrap(),
            "### [T01] Current topic\n\nSource-backed text.\n\n"
        );
        assert_eq!(
            render_revised_item(&sections[2], raw).unwrap(),
            "- [T02] **Current topic**: Source-backed text.\n\n"
        );
        for invalid in [
            r#"{"title":"Title","body":""}"#,
            r#"{"title":"First\nSecond","body":"Text"}"#,
            r#"{"title":"Title","body":"Text","topic_id":"T99"}"#,
        ] {
            assert!(render_revised_item(&sections[1], invalid).is_err());
        }
    }

    #[tokio::test]
    async fn test_item_revision_preserves_the_document_word_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let calls = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let body = if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    "word ".repeat(1000)
                } else {
                    "Source-backed repair.".into()
                };
                let item =
                    serde_json::json!({"title": "Packfile loading", "body": body}).to_string();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": item}}]
                }))
            })
            .expect(2)
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        let draft = valid_digest(&HUMAN_DIGEST_MODE, 0);
        let repaired = revise_digest_items(
            &draft,
            &["T01: repair the factual claim".into()],
            &selection_packets(4),
            &HUMAN_DIGEST_MODE,
            &backend,
            "test",
        )
        .await
        .unwrap();
        assert!(validate_digest(&repaired, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY).is_empty());
        assert!(repaired.contains("Source-backed repair."));
        assert!(repaired.contains("### [T02] object database plumbing"));
    }

    fn valid_digest(mode: &DigestMode, words: usize) -> String {
        let skeleton = "\
# Git mailing list daily digest for 2026/08/13

## The day in brief

Two series moved forward and one performance regression was diagnosed.

## Notable threads

### [T01] packfile lookup fast path

Prose about the packfile lookup fast path.

### [T02] object database plumbing

Prose about passing an explicit repository into the object database.

### [T03] completion for git history

Prose about completing subcommands of the new history command.

## In brief

- [T04] One remaining thread saw a small test fix.
";
        let target = if words == 0 {
            (mode.min_words + mode.max_words) / 2
        } else {
            words
        };
        let have = strip_topic_tags(skeleton)
            .split_whitespace()
            .filter(|token| token.chars().any(char::is_alphanumeric))
            .count();
        skeleton.replace(
            "\n\n## In brief",
            &format!(
                " {}\n\n## In brief",
                vec!["alpha"; target.saturating_sub(have)].join(" ")
            ),
        )
    }

    #[test]
    fn test_validate_digest_word_contracts() {
        for mode in [&HUMAN_DIGEST_MODE, &AI_DIGEST_MODE] {
            for words in [mode.min_words, mode.max_words] {
                assert!(
                    validate_digest(&valid_digest(mode, words), mode, &VALID_IDS, DIGEST_DAY)
                        .is_empty()
                );
            }
            for words in [mode.min_words - 1, mode.max_words + 1] {
                assert!(
                    validate_digest(&valid_digest(mode, words), mode, &VALID_IDS, DIGEST_DAY)[0]
                        .contains(&format!("word count is {words}"))
                );
            }
            let range = format!("{}-{} words", mode.min_words, mode.max_words);
            assert!(DAILY_DIGEST_AGENT.contains(&range));
            assert!(output_contract(mode, &VALID_IDS).contains(&range));
        }
    }

    #[test]
    fn test_validate_digest_counts_published_words() {
        let draft = valid_digest(&HUMAN_DIGEST_MODE, 0);
        let printed_words = strip_topic_tags(&draft)
            .split_whitespace()
            .filter(|word| word.chars().any(char::is_alphanumeric))
            .count();
        let mode = DigestMode {
            label: "human",
            min_words: printed_words + 1,
            max_words: printed_words + 100,
            ..HUMAN_DIGEST_MODE
        };
        let issues = validate_digest(&draft, &mode, &VALID_IDS, DIGEST_DAY);
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains(&format!("word count is {printed_words}")))
        );
    }

    #[test]
    fn test_validate_digest_rejects_model_chatter() {
        let base = valid_digest(&HUMAN_DIGEST_MODE, 0);
        for draft in [
            format!("Here is the revised digest:\n\n{base}"),
            format!("{base}\n\nWord count: 600. The digest follows all requirements."),
            base.replace("for 2026/08/13", "for 2026/08/12"),
            format!("{base}\n# Another title\n"),
        ] {
            assert!(
                !validate_digest(&draft, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY).is_empty(),
                "non-digest content or a wrong title must be rejected",
            );
        }
        let wrapped = base.replace(
            "One remaining thread saw a small test fix.",
            "One remaining thread saw\n  a small test fix.",
        );
        assert!(validate_digest(&wrapped, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY).is_empty(),);
    }

    #[test]
    fn test_validate_digest_rejects_broken_shape() {
        let base = valid_digest(&HUMAN_DIGEST_MODE, 0);
        let prose = "Prose about the packfile lookup fast path.";
        let subsections: String = (0..27)
            .map(|i| format!("### [T01] extra {i}\n\nProse.\n\n"))
            .collect();

        for (md, expected) in [
            (
                format!("{base}\n## Integration status\n\n- [T01] A queued topic.\n"),
                "level-2 headings",
            ),
            (
                base.replace("## The day in brief", "## Overview"),
                "level-2 headings",
            ),
            (
                base.replace("### [T03] completion for git history", "Prose."),
                "there are 2 level-3 headings",
            ),
            (
                base.replace("## In brief", &format!("{subsections}## In brief")),
                "there are 30 level-3 headings",
            ),
            (
                format!("{base}\n### [T01] an afterword\n\nProse.\n"),
                "1 level-3 heading(s) sit outside",
            ),
            (
                base.replace(
                    "### [T02] object database plumbing",
                    "### [T02] packfile lookup fast path",
                ),
                "1 level-3 heading(s) repeat",
            ),
            (
                base.replace(prose, "#### Status\n\nProse."),
                "deeper than level 3",
            ),
            (
                base.replace(
                    "### [T02] object database plumbing",
                    "### [T02] Key developments today",
                ),
                "1 generic label heading(s)",
            ),
            (
                base.replace(prose, "**Impact**: The series unblocks the reroll."),
                "1 generic label heading(s)",
            ),
            (
                base.replace(
                    prose,
                    "1. Jeff King measured it.\n2. Patrick Steinhardt agreed.",
                ),
                "2 numbered list item(s)",
            ),
            (
                base.replace(prose, "Thread root: 2026/05/22/21-35-37"),
                "`Thread root:` marker",
            ),
            (
                base.replace(prose, "**[2026/05/22]** The series was posted."),
                "`YYYY/MM/DD` date",
            ),
        ] {
            let issues = validate_digest(&md, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY);
            assert!(
                issues.iter().any(|issue| issue.contains(expected)),
                "expected `{expected}`, got {issues:?}",
            );
            assert!(issues.len() <= MAX_ISSUES);
            assert!(issues.iter().all(|issue| !issue.contains("afterword")));
        }
    }

    /// Provenance binding: every notable heading and in-brief bullet
    /// must name exactly one supplied topic packet, and no topic may be
    /// reported as quiet instead of being left out.
    #[test]
    fn test_validate_digest_requires_bound_topic_ids() {
        let base = valid_digest(&HUMAN_DIGEST_MODE, 0);

        for (md, expected) in [
            (
                base.replace(
                    "### [T02] object database plumbing",
                    "### object database plumbing",
                ),
                "do not start with the `[TNN]` ID",
            ),
            (
                base.replace("- [T04] One remaining", "- One remaining"),
                "do not start with the `[TNN]` ID",
            ),
            (
                base.replace("### [T02] object", "### [T09] object"),
                "topic ID(s) T09 are not in the evidence",
            ),
            (
                base.replace("- [T04] One remaining", "- [T01] One remaining"),
                "topic ID(s) T01 appear more than once",
            ),
            (
                base.replace(
                    "Prose about the packfile lookup fast path.",
                    "Prose mentioning [T02] inline.",
                ),
                "stray `[TNN]` topic ID(s)",
            ),
            (
                base.replace(
                    "Prose about the packfile lookup fast path.",
                    "There were no new developments here.",
                ),
                "the draft writes `no new developments`",
            ),
        ] {
            let issues = validate_digest(&md, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY);
            assert!(
                issues.iter().any(|issue| issue.contains(expected)),
                "expected `{expected}`, got {issues:?}",
            );
            assert!(issues.len() <= MAX_ISSUES);
            assert!(
                issues
                    .iter()
                    .all(|issue| !issue.contains("object database")),
                "validation issues must never quote the rejected draft: {issues:?}",
            );
        }
    }

    #[test]
    fn test_strip_topic_tags_preserves_headings_and_bullets() {
        let tagged = "## Notable threads\n\n\
                      ### [T01] packfile lookup fast path\n\n\
                      Jeff King measured a [T99] token in prose.\n\n\
                      ## In brief\n\n\
                      - [T12] A small test fix landed.\n\
                      - An untagged bullet.\n";
        assert_eq!(
            strip_topic_tags(tagged),
            "## Notable threads\n\n\
             ### packfile lookup fast path\n\n\
             Jeff King measured a [T99] token in prose.\n\n\
             ## In brief\n\n\
             - A small test fix landed.\n\
             - An untagged bullet.\n",
        );
        assert!(
            !strip_topic_tags(&valid_digest(&HUMAN_DIGEST_MODE, 0)).contains("[T0"),
            "no topic ID may survive into a published digest",
        );
    }

    #[test]
    fn test_normalize_topic_tags_keeps_only_leading_provenance() {
        let tagged = "## The day in brief\n\n\
                      [T01] A fast path was refined.\n\n\
                      ## Notable threads\n\n\
                      ### [T01] packfile [T02] lookup fast path\n\n\
                      Prose mentioning [T03] internal bookkeeping.\n\n\
                      ## In brief\n\n\
                      - [T04] A small [T05] test fix.\n";
        assert_eq!(
            normalize_topic_tags(tagged),
            "## The day in brief\n\n\
             A fast path was refined.\n\n\
             ## Notable threads\n\n\
             ### [T01] packfile lookup fast path\n\n\
             Prose mentioning internal bookkeeping.\n\n\
             ## In brief\n\n\
             - [T04] A small test fix.\n",
        );
    }

    #[test]
    fn test_normalize_topic_tags_preserves_bold_identifiers() {
        let raw = "### **[T01]** Packfile loading\n\n- **[T02]** A test fix.\n";
        let normalized = normalize_topic_tags(raw);
        assert_eq!(
            normalized,
            "### [T01] Packfile loading\n\n- [T02] A test fix.\n"
        );
        assert_eq!(
            strip_topic_tags(&normalized),
            "### Packfile loading\n\n- A test fix.\n"
        );
        assert_eq!(split_topic_tag("**[T01] unclosed").0, None);
    }

    #[test]
    fn test_parse_audit_verdict_accepts_only_a_bare_ok() {
        let lines = [
            (1, "### [T01] topic"),
            (2, ""),
            (3, "The series is ready to merge."),
        ];
        let parse = |verdict| parse_audit_verdict(verdict, "T01", &lines);
        assert!(parse("OK").unwrap().is_empty());
        assert!(parse("  OK\n").unwrap().is_empty());
        assert_eq!(
            parse("- T01 ||| L0003 ||| no delta states readiness").unwrap(),
            vec![
                "T01: line 3 `The series is ready to merge.` -- no delta states readiness"
                    .to_string()
            ],
        );
        assert!(
            parse("- T01 ||| L0099 ||| no delta states integration").is_err(),
            "an auditor may not invent the line it rejects",
        );
        for verdict in [
            "- T02 ||| L0003 ||| Wrong topic",
            "- T01 ||| L0002 ||| Blank line",
            "- T01 ||| L0000 ||| No such line",
            "- T01 ||| L0003 |||",
            "OK, with reservations",
        ] {
            assert!(parse(verdict).is_err(), "{verdict}");
        }

        let many: String = (1..=20)
            .map(|n| format!("- T01 ||| L0003 ||| unsupported claim {n}\n"))
            .collect();
        assert_eq!(parse(&many).unwrap().len(), MAX_ISSUES);
    }

    #[test]
    fn test_audit_sections_preserve_locations_and_fenced_text() {
        let draft = "# Title\n## The day in brief\nOverview.\n## Notable threads\n\
                     ### [T01] First topic\nDetails.\n```text\n### [T99] Not a topic\n```\n\
                     ## In brief\n- [T02] Second topic\n  Continued.\n";
        let sections = audit_sections(draft);
        assert_eq!(
            sections
                .iter()
                .map(|section| section.topic_id)
                .collect::<Vec<_>>(),
            vec![None, Some("T01"), Some("T02")],
        );
        assert_eq!(sections[0].lines[0], (2, "## The day in brief"));
        assert!(sections[1].lines.contains(&(8, "### [T99] Not a topic")));
        assert_eq!(sections[2].lines[0], (11, "- [T02] Second topic"));
        assert_eq!(sections[2].lines[1], (12, "  Continued."));
    }

    #[tokio::test]
    async fn test_audit_uses_source_per_topic_and_reuses_unchanged_items() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mut threads = [
            delta_activity("2026/08/12/00-00-00", &[("2026/08/13/09-00-00", "Alice")]),
            delta_activity("2026/08/12/01-00-00", &[("2026/08/13/10-00-00", "Bob")]),
        ];
        for (index, thread) in threads.iter_mut().enumerate() {
            thread.email_summaries[0].source_excerpt = Some(format!("ORIGINAL-SOURCE-{index}"));
        }
        let deltas: Vec<_> = threads
            .iter()
            .map(|thread| RootDelta {
                root_dk: thread.root_dk.clone(),
                bullets: vec!["UNTRUSTED-EXTRACTED-CLAIM".into()],
            })
            .collect();
        let packets = build_topic_packets(DIGEST_DAY, &threads, &deltas);
        let draft = "# Title\n## The day in brief\nAlpha and beta changed.\n\
                     ## Notable threads\n### [T01] Alpha\n\nAlpha changed.\n\
                     ## In brief\n- [T02] Beta changed.\n";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "OK"}}]
            })))
            .expect(6)
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        let mut approved = HashMap::new();
        for text in [
            draft.to_string(),
            draft.to_string(),
            draft.replace("Alpha changed.", "Alpha was corrected."),
        ] {
            assert!(
                audit_digest(&text, &packets, &mut approved, &backend, "test")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let requests = server.received_requests().await.unwrap();
        let inputs: Vec<String> = requests
            .iter()
            .map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                body["messages"][1]["content"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(inputs.len(), 6);
        assert!(inputs[0].contains("ORIGINAL-SOURCE-0"));
        assert!(!inputs[0].contains("ORIGINAL-SOURCE-1"));
        assert!(!inputs[0].contains("\nL0006 "));
        assert!(inputs[0].contains("\nL0007 Alpha changed."));
        assert!(inputs[1].contains("ORIGINAL-SOURCE-1"));
        assert!(!inputs[1].contains("ORIGINAL-SOURCE-0"));
        assert!(
            inputs
                .iter()
                .all(|input| !input.contains("UNTRUSTED-EXTRACTED-CLAIM"))
        );
        for index in [2, 3, 5] {
            assert!(inputs[index].contains("AUDIT LOCATION: T00"));
            assert!(inputs[index].contains("VERIFIED DIGEST ITEMS"));
            assert!(!inputs[index].contains("ORIGINAL-SOURCE"));
        }
        assert!(inputs[4].contains("AUDIT LOCATION: T01"));
        assert!(inputs[4].contains("Alpha was corrected."));
    }

    #[tokio::test]
    async fn test_rejected_audit_items_are_not_reused() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let thread = delta_activity("2026/08/12/00-00-00", &[("2026/08/13/09-00-00", "Alice")]);
        let deltas = vec![RootDelta {
            root_dk: thread.root_dk.clone(),
            bullets: vec!["A candidate.".into()],
        }];
        let packets = build_topic_packets(DIGEST_DAY, &[thread], &deltas);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "- T01 ||| L0003 ||| Unsupported assertion"
                }}]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let backend = Backend::Api {
            api_url: server.uri(),
            model: "test".into(),
            token: None,
        };
        let draft = "## Notable threads\n### [T01] Test\nUnsupported.\n";
        let mut approved = HashMap::new();
        for _ in 0..2 {
            assert_eq!(
                audit_digest(draft, &packets, &mut approved, &backend, "test")
                    .await
                    .unwrap()
                    .len(),
                1,
            );
            assert!(approved.is_empty());
        }
    }

    #[test]
    fn test_digest_validation_helpers() {
        let md = valid_digest(&HUMAN_DIGEST_MODE, 0).replace(
            "Prose about the packfile lookup fast path.",
            "Prose about the fast path.\n\n\
             ```\n\
             ### not a heading\n\
             1. not a list item\n\
             **Status**: not a label\n\
             Thread root: 2026/05/22\n\
             ```",
        );
        assert!(validate_digest(&md, &HUMAN_DIGEST_MODE, &VALID_IDS, DIGEST_DAY).is_empty());
        let raw = "Steinhardt\u{2019}s \u{201c}fast path\u{201d} cut O(N\u{00b2}) work \
                   \u{2013} 0.4s \u{2192} 4.5s \u{2014} and more\u{2026}";
        assert_eq!(
            normalize_typography(raw),
            "Steinhardt's \"fast path\" cut O(N^2) work - 0.4s -> 4.5s -- and more...",
        );
        let names = "Ren\u{e9} Scharfe and Jean-Noe\u{308}l Avila reviewed it.";
        assert_eq!(normalize_typography(names), names);

        let block = correction_block(&["word count is 977; too long".to_string()]);
        assert!(block.contains("word count is 977; too long"), "{block}");
        assert!(!block.contains("UNIQUE_DRAFT_PROSE"));
        assert!(block.contains("not shown to you"));
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

    /// A Sunday's directory also holds the week's digest, which is not
    /// an email and must not be read as one.
    #[tokio::test]
    async fn test_redo_daily_ignores_the_weekly_digest_of_a_sunday() {
        use crate::ai_backend::Backend;

        const SUNDAY: &str = "2025/01/05";
        const EMAIL: &str = "2025/01/05/09-00-00";

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let git_ref = "refs/heads/main";

        {
            let mut fi = FastImport::new(repo, git_ref).unwrap();
            fi.commit(
                "seed",
                &[
                    (&format!("{EMAIL}.md"), "Subject: A\nFrom: Alice\n\nbody"),
                    (&format!("{EMAIL}.ai.md"), "Summary of A."),
                    (&format!("{SUNDAY}/digest.weekly.ai.md"), "Weekly digest."),
                ],
            )
            .unwrap();
            fi.finish().unwrap();
        }

        let backend = Backend::Mock { nth_word: 1 };
        redo_daily(repo, git_ref, SUNDAY, &backend).await.unwrap();

        let mut cat = CatFile::new(repo).unwrap();
        assert!(
            cat.get_str(&format!("{git_ref}:{SUNDAY}/digest.human.md"))
                .is_some()
        );
    }
}
