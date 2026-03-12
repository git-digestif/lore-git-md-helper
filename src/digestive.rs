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
use crate::fast_import::FastImport;
use crate::git_util::{self, latest_digest, resolve_ref, source_commit_from_ref};
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
/// main loop.  Reset (partially) at each day boundary.
#[derive(Default)]
struct LoopState {
    prev_day: Option<String>,
    day_has_digest: bool,
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
/// Collects all shared mutable state for email summarization
/// and daily digest generation.
pub struct Digestive<'a> {
    repo_path: &'a str,
    git_ref: &'a str,
    cached: CachedReader,
    fi: Option<FastImport>,
    source_commit: Option<String>,
    backend: Option<&'a Backend>,
    dry_run: bool,
    batch_size: usize,
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
    /// day).  Emits daily digests for completed days.
    async fn finalize_day(&mut self, state: &mut LoopState, since: Option<&str>) -> Result<()> {
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
                self.finalize_day(&mut state, since).await?;

                state.prev_day = Some(day.to_string());
                state.day_has_digest = false;
                state.day_ai_exists.clear();
                state.day_email_dks.clear();
            }

            // --- File classification ---
            if let Some(dk) = path.strip_suffix(".ai.md") {
                if dk.ends_with("/digest") {
                    state.day_has_digest = true;
                } else if !dk.ends_with(".thread") {
                    state.day_ai_exists.insert(dk.to_string());
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
                }
            }
            // All other files (thread.md, thread.human.md, etc.) are skipped.
        }

        // Finalize the last day.  No digest is emitted for the last day
        // because finalize_day only emits digests for the *previous* day,
        // and there is no "next day" to trigger it.
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

    let mut user_msg = format!(
        "Date: {day}\nTotal emails today: {email_count}\nActive threads: {thread_count}\n\n",
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

    Ok(DayDigestOutput { human, ai })
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
}
