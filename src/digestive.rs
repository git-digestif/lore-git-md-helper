//! Batch AI summarization of emails in a bare git repository.
//!
//! Streams ls-tree output, looks up thread context lazily, generates
//! summaries via an AI backend, and writes results back to the
//! repository via fast-import.

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::ai_backend::Backend;
use crate::cached_reader::CachedReader;
use crate::cat_file::{BlobRead, CatFile};
use crate::date_util::{add_days, iso_sunday, month_of};
use crate::fast_import::FastImport;
use crate::git_util::{self, latest_digest, resolve_ref, source_commit_from_ref};
use crate::periodic_digest::{Granularity, SubDigest, generate_periodic_digest};
use crate::summarize::{self, EmailContext};
use crate::thread_file::{self, ThreadTree};

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
fn find_digest_commit(repo_path: &str, refname: &str, day: &str) -> Option<String> {
    let needle = format!("^digestive: daily digest for {day}$");
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

/// Summary artifacts for a single email.
pub struct SummaryFiles {
    pub dk: String,
    pub root_dk: String,
    pub human: String,
    pub ai: String,
    pub thread_human: String,
    pub thread_ai: String,
}

/// Load email content and thread context for a single email.
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

    Some(EmailContext {
        email_md,
        thread_ai_summary,
        parent_ai_summary,
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
    let result = summarize::summarize_email(&ctx, backend).await?;

    Ok(Some(SummaryFiles {
        dk: email.dk.clone(),
        root_dk: email.root_dk.clone(),
        human: result.human_summary,
        ai: result.ai_summary,
        thread_human: result.thread_human_summary,
        thread_ai: result.thread_ai_summary,
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
        })
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
        // checkpoint to land.
        let ai_key = format!("{}:{}.ai.md", self.git_ref, sf.dk);
        self.cached.insert(ai_key, sf.ai.clone());
        // Old data may contain <!-- ERROR markers from earlier runs;
        // avoid caching those as valid thread summaries.
        if !sf.thread_ai.starts_with("<!-- ERROR") {
            let key = format!("{}:{}.thread.ai.md", self.git_ref, sf.root_dk,);
            self.cached.insert(key, sf.thread_ai.clone());
        }
        self.day_summaries
            .push((sf.dk.clone(), sf.root_dk.clone(), sf.ai.clone()));
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
        let files: Vec<_> = self
            .batch
            .iter()
            .flat_map(|sf| {
                [
                    (format!("{}.human.md", sf.dk), sf.human.as_str()),
                    (
                        format!("{}.thread.human.md", sf.root_dk),
                        sf.thread_human.as_str(),
                    ),
                    (
                        format!("{}.thread.ai.md", sf.root_dk),
                        sf.thread_ai.as_str(),
                    ),
                    (format!("{}.ai.md", sf.dk), sf.ai.as_str()),
                ]
            })
            .collect();
        let refs: Vec<_> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
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
            build_day_digest_input(&self.day_summaries, &before, &mut self.cached);
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
        let cutoff = format_datekey(now - time::Duration::minutes(15));
        let cutoff_day = &cutoff[..10];
        if state
            .prev_day
            .as_deref()
            .is_some_and(|d| d < cutoff_day)
        {
            self.finalize_day(&mut state, cutoff_day, since).await?;
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

const PROJECT_CONTEXT: &str = include_str!("../prompts/git-project-context.md");

/// Per-thread data accumulated for a single day's digest.
pub struct ThreadDayActivity {
    /// Thread root date-key.
    pub root_dk: String,
    /// Thread AI summary from *before* today's emails (None if new thread).
    pub thread_ai_before: Option<String>,
    /// Today's email AI summaries, in chronological order.
    pub email_summaries: Vec<(String, String)>,
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
pub fn build_day_digest_input(
    summaries: &[(String, String, String)], // (dk, root_dk, ai_summary)
    before_commit: &str,
    cat: &mut impl BlobRead,
) -> (Vec<ThreadDayActivity>, usize) {
    use std::collections::BTreeMap;

    let mut by_thread: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (dk, root_dk, ai) in summaries {
        by_thread
            .entry(root_dk.clone())
            .or_default()
            .push((dk.clone(), ai.clone()));
    }

    let email_count = summaries.len();

    let threads: Vec<ThreadDayActivity> = by_thread
        .into_iter()
        .map(|(root_dk, emails)| {
            let spec = format!("{before_commit}:{root_dk}.thread.ai.md");
            let thread_ai_before = cat.get_str(&spec);
            ThreadDayActivity {
                root_dk,
                thread_ai_before,
                email_summaries: emails,
            }
        })
        .collect();

    (threads, email_count)
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

    // Compute the weekday name so the LLM does not have to guess it.
    let weekday = crate::date_util::parse_day(day)
        .map(|d| format!("{}", d.weekday()))
        .unwrap_or_default();

    let mut user_msg = format!(
        "Date: {day} ({weekday})\nTotal emails today: {email_count}\nActive threads: {thread_count}\n\n",
    );

    for activity in threads {
        user_msg.push_str("---\n\n");
        user_msg.push_str(&format!("Thread root: {}\n\n", activity.root_dk));

        if let Some(ref before) = activity.thread_ai_before {
            user_msg.push_str("Previous thread state (before today):\n\n");
            user_msg.push_str(before);
            user_msg.push_str("\n\n");
        }

        user_msg.push_str("Today's new emails in this thread:\n\n");
        for (dk, ai) in &activity.email_summaries {
            user_msg.push_str(&format!("[{dk}]\n{ai}\n\n"));
        }
    }

    eprintln!(
        "[digestive] generating daily digest for {day} ({email_count} emails, {thread_count} threads) ...",
    );

    let system = format!("{DAILY_DIGEST_AGENT}\n\n{PROJECT_CONTEXT}");

    let human = backend
        .chat_with_options(&system, &format!("Mode: human\n\n{user_msg}"), Some(0.0))
        .await
        .context("daily digest (human) failed")?;

    let ai = backend
        .chat_with_options(&system, &format!("Mode: ai\n\n{user_msg}"), Some(0.0))
        .await
        .context("daily digest (AI) failed")?;

    Ok(DayDigestOutput {
        human: summarize::normalize_headings(&human),
        ai,
    })
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
        let summaries = vec![(
            "2025/01/06/10-00-00".into(),
            "2025/01/06/10-00-00".into(),
            "email ai".into(),
        )];
        let (threads, count) = build_day_digest_input(&summaries, "before", &mut blobs);
        assert_eq!(count, 1);
        assert_eq!(threads.len(), 1);
        assert!(
            threads[0].thread_ai_before.is_none(),
            "new thread should have no before state"
        );
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
        let (threads, _) = build_day_digest_input(&summaries, "before", &mut blobs);
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
        let (threads, _) = build_day_digest_input(&summaries, "before", &mut blobs);
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
        let (threads, count) = build_day_digest_input(&summaries, "before", &mut blobs);
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
}
