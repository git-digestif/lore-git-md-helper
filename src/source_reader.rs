use anyhow::Result;

use crate::cat_file::CatFile;
use crate::git_util::git;

/// A commit OID and the raw email content from the source repo.
pub struct SourceEmail {
    pub commit_oid: String,
    pub raw_email: Vec<u8>,
}

/// Read emails from a lore-git source repo, oldest first.
///
/// Uses `git rev-list --reverse` to enumerate commits and `CatFile`
/// to read the blob at `<commit>:m` for each one.
///
/// `range` is passed directly to `git rev-list` — e.g. `"HEAD"`,
/// `"abc123..def456"`, or `"abc123..HEAD"`.
pub fn read_source_emails(repo_path: &str, range: &str) -> Result<Vec<SourceEmail>> {
    let commits: Vec<String> = git(repo_path, &["rev-list", "--reverse", range])?
        .lines()
        .map(str::to_owned)
        .collect();

    if commits.is_empty() {
        return Ok(Vec::new());
    }

    eprintln!("reading {} emails from {repo_path}...", commits.len());

    let mut cat = CatFile::new(repo_path)?;
    let mut emails = Vec::with_capacity(commits.len());

    for commit_oid in &commits {
        let spec = format!("{commit_oid}:m");
        match cat.get(&spec) {
            Some(data) => {
                emails.push(SourceEmail {
                    commit_oid: commit_oid.clone(),
                    raw_email: data,
                });
            }
            None => {
                eprintln!("warning: {spec} missing, skipping");
            }
        }
    }

    Ok(emails)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fast_import::FastImport;
    use crate::git_util::resolve_ref;
    use crate::git_util::tests::init_bare_repo;

    fn seed_source_repo(raw_email: &str) -> (tempfile::TempDir, String, String) {
        let dir = init_bare_repo();
        let repo = dir.path().to_str().unwrap().to_string();

        let mut fi = FastImport::new(&repo, "refs/heads/main").unwrap();
        fi.commit("seed email", &[("m", raw_email)]).unwrap();
        fi.finish().unwrap();

        let commit_oid = resolve_ref(&repo, "refs/heads/main").unwrap();
        (dir, repo, commit_oid)
    }

    #[test]
    fn test_read_source_emails_reads_head_commit() {
        let (_dir, repo, commit_oid) =
            seed_source_repo("From: test@example.com\nSubject: Test\n\nHello.\n");

        let emails = read_source_emails(&repo, "HEAD").unwrap();

        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].commit_oid, commit_oid);
        assert_eq!(
            emails[0].raw_email,
            b"From: test@example.com\nSubject: Test\n\nHello.\n"
        );
    }

    #[test]
    fn test_read_source_emails_invalid_range_is_error() {
        let (_dir, repo, _commit_oid) = seed_source_repo("From: test@example.com\n\nHello.\n");

        let err = match read_source_emails(&repo, "does-not-exist..HEAD") {
            Ok(emails) => panic!("expected error, got {} emails", emails.len()),
            Err(err) => err,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("rev-list"), "unexpected error: {msg}");
        assert!(msg.contains("does-not-exist"), "unexpected error: {msg}");
    }
}
