//! Small Git helper utilities shared across binaries.

use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// Builder for running git commands against a bare repository.
pub struct GitCommand<'a> {
    repo_path: &'a str,
    args: &'a [&'a str],
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

/// A spawned git child process with checked wait helpers.
pub struct GitChild {
    child: Option<Child>,
    command: String,
}

/// Run a git command against a bare repository.
///
/// Returns trimmed stdout on success, or a descriptive error on failure.
pub fn git(repo_path: &str, args: &[&str]) -> Result<String> {
    GitCommand::new(repo_path, args).run()
}

impl<'a> GitCommand<'a> {
    pub fn new(repo_path: &'a str, args: &'a [&'a str]) -> Self {
        Self {
            repo_path,
            args,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn stdin(mut self, stdin: Stdio) -> Self {
        self.stdin = Some(stdin);
        self
    }

    pub fn stdout(mut self, stdout: Stdio) -> Self {
        self.stdout = Some(stdout);
        self
    }

    pub fn stderr(mut self, stderr: Stdio) -> Self {
        self.stderr = Some(stderr);
        self
    }

    pub fn run(self) -> Result<String> {
        let output = self.output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn output(self) -> Result<Output> {
        let repo_path = self.repo_path;
        let args = self.args;
        let output = self
            .into_command()
            .output()
            .with_context(|| format!("failed to run {}", render_git_command(repo_path, args)))?;
        checked_output(|| render_git_command(repo_path, args), output)
    }

    pub fn spawn(self) -> Result<GitChild> {
        let repo_path = self.repo_path;
        let args = self.args;
        let command = render_git_command(repo_path, args);
        let child = self
            .into_command()
            .spawn()
            .with_context(|| format!("failed to spawn {command}"))?;
        Ok(GitChild {
            child: Some(child),
            command,
        })
    }

    fn into_command(self) -> Command {
        let mut command = Command::new("git");
        command.args(["--git-dir", self.repo_path]).args(self.args);
        if let Some(stdin) = self.stdin {
            command.stdin(stdin);
        }
        if let Some(stdout) = self.stdout {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr {
            command.stderr(stderr);
        }
        command
    }
}

impl GitChild {
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn wait_with_output(mut self) -> Result<Output> {
        let child = self.child.take().expect("GitChild already consumed");
        let command = std::mem::take(&mut self.command);
        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for {command}"))?;
        checked_output(|| command, output)
    }
}

impl Drop for GitChild {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.wait();
        }
    }
}

fn checked_output<F>(render_command: F, output: Output) -> Result<Output>
where
    F: FnOnce() -> String,
{
    if output.status.success() {
        return Ok(output);
    }

    let command = render_command();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        bail!("{command} failed with {}", output.status);
    }
    bail!("{command} failed with {}: {stderr}", output.status);
}

fn render_git_command(repo_path: &str, args: &[&str]) -> String {
    use std::fmt::Write;

    let mut command = String::from("git");
    write!(&mut command, " --git-dir {:?}", repo_path).unwrap();
    for arg in args {
        write!(&mut command, " {:?}", arg).unwrap();
    }
    command
}

/// Resolve a ref to its full OID.
pub fn resolve_ref(repo_path: &str, refname: &str) -> Option<String> {
    git(repo_path, &["rev-parse", "--verify", refname]).ok()
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    pub fn git(git_dir: &str, args: &[&str]) -> String {
        super::git(git_dir, args)
            .unwrap_or_else(|e| panic!("git --git-dir {git_dir} {args:?} failed: {e:#}"))
    }

    pub fn init_bare_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        git(p, &["init", "--bare", "-b", "main"]);
        dir
    }

    #[test]
    fn git_child_wait_reports_nonzero_exit() {
        use super::*;
        let dir = init_bare_repo();
        let p = dir.path().to_str().unwrap();
        let err = GitCommand::new(p, &["rev-parse", "--verify", "does-not-exist"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rev-parse"), "unexpected error: {msg}");
        assert!(msg.contains("does-not-exist"), "unexpected error: {msg}");
    }
}
