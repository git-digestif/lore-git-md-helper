//! Write side of the fast-import protocol, mirroring `CatFile` on
//! the read side.
//!
//! Wraps a `git fast-import --quiet` child process (or any `Write`
//! sink for testing).  Callers create commits via `commit()` and
//! never need to know the fast-import protocol syntax.
//!
//! A single fast-import process can write to multiple refs.  Use
//! `sibling()` to obtain a second handle that shares the same
//! process and mark counter but tracks its own ref and parent chain.

use std::cell::RefCell;
use std::io::Write;
use std::process::Stdio;
use std::rc::Rc;

use anyhow::Result;

use crate::git_util::{GitChild, GitCommand};

/// Shared state between sibling `FastImport` handles: the output
/// stream, the child process (if any), and the mark counter.
struct FastImportStream {
    out: Box<dyn Write>,
    child: Option<GitChild>,
    mark: u64,
}

/// A fast-import handle bound to a single Git ref.
///
/// For production use, create via `FastImport::new()` which spawns
/// `git fast-import`.  For testing, use `FastImport::from_writer()`
/// with a `Vec<u8>` or similar sink.  Use `sibling()` to create a
/// second handle writing to a different ref on the same stream.
pub struct FastImport {
    stream: Rc<RefCell<FastImportStream>>,
    parent: Option<String>,
    refname: String,
}

impl FastImport {
    /// Spawn `git --git-dir=<repo> fast-import --quiet` and write
    /// commits to `refname`.
    pub fn new(repo_path: &str, refname: &str) -> Result<Self> {
        let mut child = GitCommand::new(repo_path, &["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .spawn()?;

        let stdin = child.take_stdin().unwrap();

        Ok(FastImport {
            stream: Rc::new(RefCell::new(FastImportStream {
                out: Box::new(stdin),
                child: Some(child),
                mark: 0,
            })),
            parent: None,
            refname: refname.to_string(),
        })
    }

    /// Create a `FastImport` that writes to an arbitrary `Write`
    /// sink (useful for testing).
    pub fn from_writer(out: impl Write + 'static, refname: &str) -> Self {
        FastImport {
            stream: Rc::new(RefCell::new(FastImportStream {
                out: Box::new(out),
                child: None,
                mark: 0,
            })),
            parent: None,
            refname: refname.to_string(),
        }
    }

    /// Create a sibling handle that writes to a different ref on the
    /// same fast-import stream.  The sibling shares the mark counter
    /// (marks never collide) and the underlying process, but tracks
    /// its own parent chain independently.
    pub fn sibling(&self, refname: &str) -> Self {
        FastImport {
            stream: Rc::clone(&self.stream),
            parent: None,
            refname: refname.to_string(),
        }
    }

    /// Set the initial parent commit (e.g. the current tip of the
    /// ref).  If not called, the first commit has no parent.
    pub fn set_parent(&mut self, oid: String) {
        self.parent = Some(oid);
    }

    /// Write a single commit containing the given files.
    ///
    /// Each element of `files` is a `(path, content)` pair.
    /// Returns the mark of the new commit, or 0 when both `files` and
    /// `symlinks` are empty (no commit written).
    pub fn commit(&mut self, msg: &str, files: &[(&str, &str)]) -> Result<u64> {
        self.commit_with_symlinks(msg, files, &[], &[])
    }

    /// Write a single commit containing regular files and symlinks.
    ///
    /// Regular `files` are `(path, content)` pairs (mode `100644`).
    /// `symlinks` are `(path, target)` pairs (mode `120000`).
    /// `deletes` are paths to remove from the tree.
    /// Returns the mark of the new commit, or 0 when all slices are
    /// empty (no commit written).
    pub fn commit_with_symlinks(
        &mut self,
        msg: &str,
        files: &[(&str, &str)],
        symlinks: &[(&str, &str)],
        deletes: &[&str],
    ) -> Result<u64> {
        if files.is_empty() && symlinks.is_empty() && deletes.is_empty() {
            return Ok(0);
        }

        let mut s = self.stream.borrow_mut();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        s.mark += 1;
        let m = s.mark;

        writeln!(s.out, "commit {}", self.refname)?;
        writeln!(s.out, "mark :{m}")?;
        writeln!(s.out, "committer lore-git-md <> {now} +0000")?;
        writeln!(s.out, "data {}", msg.len())?;
        s.out.write_all(msg.as_bytes())?;
        writeln!(s.out)?;

        if let Some(p) = self.parent.as_deref() {
            writeln!(s.out, "from {p}")?;
        }
        self.parent = Some(format!(":{m}"));

        // Deletes first so that a path appearing in both deletes and
        // files/symlinks results in the file being recreated (the last
        // command for a path wins in fast-import).
        for path in deletes {
            writeln!(s.out, "D {path}")?;
        }

        for (path, content) in files {
            let data = content.as_bytes();
            writeln!(s.out, "M 100644 inline {path}")?;
            writeln!(s.out, "data {}", data.len())?;
            s.out.write_all(data)?;
            writeln!(s.out)?;
        }

        for (path, target) in symlinks {
            let data = target.as_bytes();
            writeln!(s.out, "M 120000 inline {path}")?;
            writeln!(s.out, "data {}", data.len())?;
            s.out.write_all(data)?;
            writeln!(s.out)?;
        }

        Ok(m)
    }

    /// Emit a `checkpoint` command so that refs are updated and
    /// the data written so far is queryable.
    pub fn checkpoint(&mut self) -> Result<()> {
        let mut s = self.stream.borrow_mut();
        writeln!(s.out, "checkpoint")?;
        s.out.flush()?;
        Ok(())
    }

    /// Write `done`, close stdin, and wait for the child to exit.
    /// Returns an error if fast-import exits non-zero.
    ///
    /// After `finish()`, any sibling handles are invalidated (writes
    /// will fail because the stream is closed).
    pub fn finish(self) -> Result<()> {
        let mut s = self.stream.borrow_mut();
        writeln!(s.out, "done")?;
        s.out = Box::new(std::io::sink());
        if let Some(child) = s.child.take() {
            child.wait_with_output()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Shared buffer for testing: implements `Write` and lets the
    /// test read back what was written after the `FastImport` is done.
    #[derive(Clone)]
    struct TestBuf(Rc<RefCell<Vec<u8>>>);

    impl TestBuf {
        fn new() -> Self {
            TestBuf(Rc::new(RefCell::new(Vec::new())))
        }
        fn output(&self) -> String {
            String::from_utf8_lossy(&self.0.borrow()).to_string()
        }
    }

    impl Write for TestBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    #[test]
    fn test_single_commit() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit("test message", &[("file.md", "hello")]).unwrap();

        let out = buf.output();
        assert!(out.contains("commit refs/heads/test\n"));
        assert!(out.contains("mark :1\n"));
        assert!(out.contains("data 12\n")); // "test message".len()
        assert!(out.contains("test message\n"));
        assert!(out.contains("M 100644 inline file.md\n"));
        assert!(out.contains("data 5\n")); // "hello".len()
        assert!(out.contains("hello\n"));
        assert!(!out.contains("\nfrom "));
    }

    #[test]
    fn test_parent_chaining() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit("first", &[("a.md", "a")]).unwrap();
        fi.commit("second", &[("b.md", "b")]).unwrap();

        let out = buf.output();
        assert!(out.contains("mark :1\n"));
        assert!(out.contains("mark :2\n"));
        assert!(out.contains("from :1\n"));
    }

    #[test]
    fn test_explicit_parent() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.set_parent("abc123".into());
        fi.commit("msg", &[("f.md", "x")]).unwrap();
        let out = buf.output();
        assert!(out.contains("from abc123\n"));
    }

    #[test]
    fn test_checkpoint() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit("msg", &[("f.md", "x")]).unwrap();
        fi.checkpoint().unwrap();

        let out = buf.output();
        assert!(out.contains("checkpoint\n"));
    }

    #[test]
    fn test_empty_files_skipped() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit("msg", &[]).unwrap();

        let out = buf.output();
        assert!(out.is_empty(), "empty file list should produce no output");
    }

    #[test]
    fn test_multiple_files_in_one_commit() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit(
            "multi",
            &[("a.md", "aaa"), ("b.md", "bbb"), ("c.md", "ccc")],
        )
        .unwrap();

        let out = buf.output();
        assert!(out.contains("M 100644 inline a.md\n"));
        assert!(out.contains("M 100644 inline b.md\n"));
        assert!(out.contains("M 100644 inline c.md\n"));
        assert_eq!(out.matches("commit refs/heads/test").count(), 1);
    }

    #[test]
    fn test_symlinks() {
        let buf = TestBuf::new();
        let mut fi = FastImport::from_writer(buf.clone(), "refs/heads/test");
        fi.commit_with_symlinks(
            "with links",
            &[("file.md", "content")],
            &[("link.md", "file.md")],
            &[],
        )
        .unwrap();

        let out = buf.output();
        assert!(out.contains("M 100644 inline file.md\n"));
        assert!(out.contains("M 120000 inline link.md\n"));
    }

    #[test]
    fn test_sibling_shared_marks() {
        let buf = TestBuf::new();
        let mut fi_main = FastImport::from_writer(buf.clone(), "refs/heads/main");
        let mut fi_notes = fi_main.sibling("refs/notes/msgid");

        fi_main.set_parent("HEAD".into());
        fi_notes.set_parent("refs/notes/msgid".into());

        let m1 = fi_main.commit("content", &[("a.md", "a")]).unwrap();
        let m2 = fi_notes.commit("notes", &[("ab/cd/ef", "note")]).unwrap();
        let m3 = fi_main.commit("more content", &[("b.md", "b")]).unwrap();

        assert_eq!(m1, 1);
        assert_eq!(m2, 2);
        assert_eq!(m3, 3);

        let out = buf.output();
        // main's first commit parents from HEAD
        assert!(out.contains("commit refs/heads/main\nmark :1\n"));
        assert!(out.contains("from HEAD\n"));
        // notes commit parents from its own ref
        assert!(out.contains("commit refs/notes/msgid\nmark :2\n"));
        assert!(out.contains("from refs/notes/msgid\n"));
        // main's second commit parents from :1 (its own chain)
        assert!(out.contains("commit refs/heads/main\nmark :3\n"));
        assert!(out.contains("from :1\n"));
    }
}
