//! Small Git helper utilities shared across binaries.

use std::process::Command;

use anyhow::{Result, bail};

/// Run a git command against a bare repository.
///
/// Returns trimmed stdout on success or an error containing the
/// command line and stderr.
pub fn git(repo_path: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(["--git-dir", repo_path])
        .args(args)
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git --git-dir {repo_path} {} failed: {stderr}",
            args.join(" ")
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve a ref to its full OID.
pub fn resolve_ref(repo_path: &str, refname: &str) -> Option<String> {
    git(repo_path, &["rev-parse", "--verify", refname]).ok()
}

#[cfg(test)]
pub(crate) mod tests {}
