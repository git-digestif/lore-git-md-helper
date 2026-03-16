use std::collections::HashSet;

use anyhow::Result;
use time::OffsetDateTime;

use crate::git_util::git;

/// Convert a Unix timestamp to a UTC date-key: `YYYY/MM/DD/HH-MM-SS`.
///
/// If `existing` already contains that key, appends `-1`, `-2`, … to resolve
/// the collision. The returned key is inserted into `existing`.
pub fn date_to_key_from_timestamp(ts: i64, existing: &mut HashSet<String>) -> Result<String> {
    let utc = OffsetDateTime::from_unix_timestamp(ts)?;
    resolve_key(utc, existing)
}

/// Convert an RFC 2822 date string to a UTC date-key: `YYYY/MM/DD/HH-MM-SS`.
///
/// If `existing` already contains that key, appends `-1`, `-2`, … to resolve
/// the collision. The returned key is inserted into `existing`.
pub fn date_to_key(date_rfc2822: &str, existing: &mut HashSet<String>) -> Result<String> {
    let dt = mail_parser::DateTime::parse_rfc822(date_rfc2822)
        .ok_or_else(|| anyhow::anyhow!("cannot parse date: {date_rfc2822}"))?;

    let utc = OffsetDateTime::from_unix_timestamp(dt.to_timestamp())?;
    resolve_key(utc, existing)
}

fn resolve_key(utc: OffsetDateTime, existing: &mut HashSet<String>) -> Result<String> {
    let base = format!(
        "{:04}/{:02}/{:02}/{:02}-{:02}-{:02}",
        utc.year(),
        utc.month() as u8,
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    );

    if existing.insert(base.clone()) {
        return Ok(base);
    }

    for i in 1u32.. {
        let candidate = format!("{base}-{i}");
        if existing.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    unreachable!()
}

/// Scan the target repo's tree for existing `.md` files and return their
/// date-keys as a `HashSet`, ready for use with `date_to_key()`.
///
/// Runs `git ls-tree -r --name-only refs/heads/main` and strips the
/// `.md` suffix.  Ignores `.thread.md` files (those are not date-keys).
/// Returns an empty set if the ref does not exist yet (fresh repo).
pub fn load_existing_keys(repo_path: &str) -> Result<HashSet<String>> {
    use crate::git_util::resolve_ref;

    if resolve_ref(repo_path, "refs/heads/main").is_none() {
        return Ok(HashSet::new());
    }

    let stdout = git(
        repo_path,
        &["ls-tree", "-r", "--name-only", "refs/heads/main"],
    )?;

    let mut keys = HashSet::new();
    for path in stdout.lines() {
        if path.ends_with(".thread.md") {
            continue;
        }
        if let Some(key) = path.strip_suffix(".md") {
            keys.insert(key.to_string());
        }
    }

    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_to_key_basic() {
        let mut existing = HashSet::new();
        let key = date_to_key("Wed, 12 Feb 2025 09:40:17 +0530", &mut existing).unwrap();
        assert_eq!(key, "2025/02/12/04-10-17");
    }

    #[test]
    fn test_date_to_key_utc() {
        let mut existing = HashSet::new();
        let key = date_to_key("Mon, 10 Feb 2025 00:00:00 +0000", &mut existing).unwrap();
        assert_eq!(key, "2025/02/10/00-00-00");
    }

    #[test]
    fn test_date_to_key_negative_offset() {
        let mut existing = HashSet::new();
        let key = date_to_key("Mon, 10 Feb 2025 20:00:00 -0800", &mut existing).unwrap();
        assert_eq!(key, "2025/02/11/04-00-00");
    }

    #[test]
    fn test_date_to_key_collision() {
        let mut existing = HashSet::new();
        let k1 = date_to_key("Mon, 10 Feb 2025 00:00:00 +0000", &mut existing).unwrap();
        assert_eq!(k1, "2025/02/10/00-00-00");

        let k2 = date_to_key("Mon, 10 Feb 2025 00:00:00 +0000", &mut existing).unwrap();
        assert_eq!(k2, "2025/02/10/00-00-00-1");

        let k3 = date_to_key("Mon, 10 Feb 2025 00:00:00 +0000", &mut existing).unwrap();
        assert_eq!(k3, "2025/02/10/00-00-00-2");
    }

    #[test]
    fn test_date_to_key_year_boundary() {
        let mut existing = HashSet::new();
        let key = date_to_key("Tue, 31 Dec 2024 23:30:00 -0100", &mut existing).unwrap();
        assert_eq!(key, "2025/01/01/00-30-00");
    }

    #[test]
    fn test_load_existing_keys_empty_repo() {
        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();
        let keys = load_existing_keys(repo).unwrap();
        assert!(keys.is_empty());
    }

    #[test]
    fn test_load_existing_keys_with_files() {
        use crate::fast_import::FastImport;

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();

        let mut fi = FastImport::new(repo, "refs/heads/main").unwrap();
        fi.commit(
            "seed",
            &[
                ("2025/02/12/04-10-17.md", "email one"),
                ("2025/02/10/00-00-00.md", "email two"),
                ("2025/02/10/00-00-00.thread.md", "thread summary"),
            ],
        )
        .unwrap();
        fi.finish().unwrap();

        let keys = load_existing_keys(repo).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("2025/02/12/04-10-17"));
        assert!(keys.contains("2025/02/10/00-00-00"));
    }

    #[test]
    fn test_load_existing_keys_excludes_thread_files() {
        use crate::fast_import::FastImport;

        let dir = crate::git_util::tests::init_bare_repo();
        let repo = dir.path().to_str().unwrap();

        let mut fi = FastImport::new(repo, "refs/heads/main").unwrap();
        fi.commit(
            "seed",
            &[
                ("2025/03/01/12-00-00.md", "email"),
                ("2025/03/01/12-00-00.thread.md", "thread"),
            ],
        )
        .unwrap();
        fi.finish().unwrap();

        let keys = load_existing_keys(repo).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("2025/03/01/12-00-00"));
        assert!(!keys.contains("2025/03/01/12-00-00.thread"));
    }
}
