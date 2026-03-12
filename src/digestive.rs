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
use crate::git_util::{self, resolve_ref, source_commit_from_ref};
use crate::summarize::{self, EmailContext};
use crate::thread_file;

/// An email in the date range, with its thread root and summary status.
pub struct EmailToSummarize {
    pub dk: String,
    pub root_dk: String,
}

/// Lazily resolve the thread root for a date-key.
///
/// Checks `thread_roots` cache first.  On miss, loads the `.thread.md`
/// file via CatFile (which follows symlinks for replies) and parses
/// it to extract the root date-key.  Falls back to `dk` itself if no
/// thread file exists (standalone email).
fn resolve_thread_root(
    dk: &str,
    cached: &mut impl BlobRead,
    git_ref: &str,
    thread_roots: &mut HashMap<String, String>,
) -> String {
    if let Some(root) = thread_roots.get(dk) {
        return root.clone();
    }
    let root_dk = thread_file::load_from_repo(cached, git_ref, dk)
        .map(|(root, _)| root)
        .unwrap_or_else(|| dk.to_string());
    thread_roots.insert(dk.to_string(), root_dk.clone());
    root_dk
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

    Some(EmailContext {
        email_md,
        thread_ai_summary,
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

/// Pipeline state for the digestive batch processor.
///
/// Collects all shared mutable state for email summarization
/// and fast-import output.
pub struct Digestive<'a> {
    repo_path: &'a str,
    git_ref: &'a str,
    cached: CachedReader,
    fi: Option<FastImport>,
    source_commit: Option<String>,
    backend: Option<&'a Backend>,
    dry_run: bool,
    batch_size: usize,
    total_processed: u64,
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

        Ok(Digestive {
            repo_path,
            git_ref,
            cached,
            fi,
            source_commit,
            backend,
            dry_run,
            batch_size,
            total_processed: 0,
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
        label: Option<&str>,
    ) -> Result<bool> {
        let email = EmailToSummarize {
            dk: dk.to_string(),
            root_dk: root_dk.to_string(),
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
            if let Err(e) = self
                .summarize_and_record(dk, root_dk, Some("backfilling"))
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
    /// inline as they are discovered during the ls-tree scan.
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

        let mut ai_exists: HashSet<String> = HashSet::new();
        let mut thread_roots: HashMap<String, String> = HashMap::new();

        for path in stdout.lines() {
            // --- File classification ---
            if let Some(dk) = path.strip_suffix(".ai.md") {
                if !dk.ends_with(".thread") {
                    ai_exists.insert(dk.to_string());
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

                // Summarize in-range emails that lack an AI summary.
                if in_range(dk) && !ai_exists.contains(dk) {
                    let root_dk =
                        resolve_thread_root(dk, &mut self.cached, self.git_ref, &mut thread_roots);

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

                    self.summarize_and_record(dk, &root_dk, None).await?;
                }
            }
            // All other files (thread.md, thread.human.md, etc.) are skipped.
        }

        self.flush_batch()?;

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
        };
        let ctx = load_email_context(&email, &mut blobs, "main").unwrap();
        assert_eq!(
            ctx.thread_ai_summary.as_deref(),
            Some("cached thread"),
            "in-memory cache should take precedence over repo"
        );
    }
}
