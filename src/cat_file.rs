//! Persistent `git cat-file --batch` process for efficient object lookups.
//!
//! Wraps a single long-running `git cat-file --batch` child process.
//! Callers write object specs (e.g. `HEAD:path/to/file` or a raw OID)
//! and read back the content, avoiding per-lookup process spawn
//! overhead.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, Stdio};

use anyhow::Result;

use crate::git_util::{GitChild, GitCommand};

/// Trait for reading blob content by spec (e.g. "HEAD:path/to/file").
pub trait BlobRead {
    fn get_str(&mut self, spec: &str) -> Option<String>;
}

pub struct CatFile {
    /// Kept for its `Drop` impl, which waits for the child process.
    _child: GitChild,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
}

impl CatFile {
    /// Spawn a persistent `git cat-file --batch` process against
    /// `repo_path`.
    pub fn new(repo_path: &str) -> Result<Self> {
        let mut child = GitCommand::new(repo_path, &["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdin = child.take_stdin().unwrap();
        let stdout = BufReader::new(child.take_stdout().unwrap());

        Ok(Self {
            _child: child,
            stdin: Some(stdin),
            stdout,
        })
    }

    /// Query an object by spec (e.g. `"HEAD:2025/01/01/00-00-00.thread.md"`
    /// or a raw SHA).  Returns `None` if the object is missing.
    pub fn get(&mut self, spec: &str) -> Option<Vec<u8>> {
        let stdin = self.stdin.as_mut()?;
        writeln!(stdin, "{spec}").ok()?;
        stdin.flush().ok()?;

        let mut header = String::new();
        self.stdout.read_line(&mut header).ok()?;
        let header = header.trim_end();

        if header.contains("missing") {
            return None;
        }

        let size: usize = header.rsplit_once(' ')?.1.parse().ok()?;
        let mut buf = vec![0u8; size];
        self.stdout
            .read_exact(&mut buf)
            .expect("cat-file read desync");
        // Consume the trailing newline after the content
        let mut nl = [0u8; 1];
        self.stdout
            .read_exact(&mut nl)
            .expect("cat-file trailing newline desync");

        Some(buf)
    }

    /// Query and return content as a UTF-8 string, or `None` if missing.
    pub fn get_str(&mut self, spec: &str) -> Option<String> {
        let bytes = self.get(spec)?;
        String::from_utf8(bytes).ok()
    }
}

impl BlobRead for CatFile {
    fn get_str(&mut self, spec: &str) -> Option<String> {
        self.get_str(spec)
    }
}

/// In-memory blob store for testing, keyed by spec string.
pub struct MockBlobs(pub std::collections::HashMap<String, String>);

impl BlobRead for MockBlobs {
    fn get_str(&mut self, spec: &str) -> Option<String> {
        self.0.get(spec).cloned()
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        // Close stdin so cat-file sees EOF; GitChild::drop() waits.
        self.stdin.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast_import::FastImport;
    use crate::git_util::tests::init_bare_repo;

    fn seed_repo(dir: &tempfile::TempDir, files: &[(&str, &str)]) {
        let p = dir.path().to_str().unwrap();
        let mut fi = FastImport::new(p, "refs/heads/main").unwrap();
        fi.commit("seed", files).unwrap();
        fi.finish().unwrap();
    }

    #[test]
    fn test_get_existing_blob() {
        let dir = init_bare_repo();
        seed_repo(&dir, &[("m", "hello world\n")]);
        let p = dir.path().to_str().unwrap();
        let mut cf = CatFile::new(p).unwrap();
        let data = cf.get("main:m").expect("blob should exist");
        assert_eq!(data, b"hello world\n");
    }

    #[test]
    fn test_get_missing_returns_none() {
        let dir = init_bare_repo();
        seed_repo(&dir, &[("m", "x")]);
        let p = dir.path().to_str().unwrap();
        let mut cf = CatFile::new(p).unwrap();
        assert!(cf.get("main:no-such-file").is_none());
    }

    #[test]
    fn test_get_str_returns_utf8() {
        let dir = init_bare_repo();
        seed_repo(&dir, &[("m", "café\n")]);
        let p = dir.path().to_str().unwrap();
        let mut cf = CatFile::new(p).unwrap();
        let s = cf.get_str("main:m").expect("should return string");
        assert_eq!(s, "café\n");
    }

    #[test]
    fn test_multiple_sequential_queries() {
        let dir = init_bare_repo();
        seed_repo(&dir, &[("a", "alpha"), ("b", "bravo"), ("c", "charlie")]);
        let p = dir.path().to_str().unwrap();
        let mut cf = CatFile::new(p).unwrap();
        assert_eq!(cf.get_str("main:a").unwrap(), "alpha");
        assert!(cf.get_str("main:missing").is_none());
        assert_eq!(cf.get_str("main:b").unwrap(), "bravo");
        assert_eq!(cf.get_str("main:c").unwrap(), "charlie");
        // Repeat a query to confirm re-reads work after a miss
        assert_eq!(cf.get_str("main:a").unwrap(), "alpha");
    }

    #[test]
    fn test_drop_does_not_panic() {
        let dir = init_bare_repo();
        seed_repo(&dir, &[("m", "x")]);
        let p = dir.path().to_str().unwrap();
        let cf = CatFile::new(p).unwrap();
        drop(cf);
    }
}
