use std::collections::HashMap;

use anyhow::{Context, Result};

/// List all `.md` blobs (excluding `.thread.md`) in the tree of
/// `git_ref`.  Returns a map of `path -> blob SHA`.
///
/// Calls `on_entry(count, path)` for each entry so callers can display
/// scanning progress on large trees.  The entries stream directly from
/// a `git ls-tree -r` child process, so progress updates appear
/// continuously rather than only after the full output has been
/// buffered.
pub fn ls_tree(
    repo: &str,
    git_ref: &str,
    mut on_entry: impl FnMut(usize, &str),
) -> Result<HashMap<String, String>> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    use crate::git_util::GitCommand;

    let mut child = GitCommand::new(repo, &["ls-tree", "-r", git_ref])
        .stdout(Stdio::piped())
        .spawn()
        .context(format!("git ls-tree {git_ref} failed"))?;

    let stdout = child.take_stdout().context("no stdout from ls-tree")?;
    let reader = BufReader::new(stdout);

    let mut map = HashMap::new();
    for line in reader.lines() {
        let line = line.context("reading ls-tree output")?;
        // "<mode> <type> <sha>\t<path>"
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        if !path.ends_with(".md") || path.ends_with(".thread.md") {
            continue;
        }
        let sha = meta.split_whitespace().nth(2).unwrap_or("").to_owned();
        map.insert(path.to_owned(), sha);
        on_entry(map.len(), path);
    }

    child.wait_with_output()?;
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast_import::FastImport;
    use crate::git_util::tests::init_bare_repo;

    #[test]
    fn ls_tree_filters_thread_files() {
        let dir = init_bare_repo();
        let repo = dir.path().to_str().unwrap();

        let mut fi = FastImport::new(repo, "refs/heads/main").unwrap();
        fi.commit(
            "seed",
            &[
                ("2025/01/01/00-00-00.md", "email content"),
                ("2025/01/01/00-00-00.thread.md", "thread content"),
                ("2025/01/02/10-00-00.md", "another email"),
                ("README.md", "not an email"),
            ],
        )
        .unwrap();
        fi.finish().unwrap();

        let tree = ls_tree(repo, "refs/heads/main", |_, _| {}).unwrap();

        assert!(tree.contains_key("2025/01/01/00-00-00.md"));
        assert!(tree.contains_key("2025/01/02/10-00-00.md"));
        assert!(tree.contains_key("README.md"));
        assert!(
            !tree.contains_key("2025/01/01/00-00-00.thread.md"),
            ".thread.md should be excluded"
        );
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn ls_tree_returns_blob_shas() {
        let dir = init_bare_repo();
        let repo = dir.path().to_str().unwrap();

        let mut fi = FastImport::new(repo, "refs/heads/main").unwrap();
        fi.commit("seed", &[("test.md", "hello")]).unwrap();
        fi.finish().unwrap();

        let tree = ls_tree(repo, "refs/heads/main", |_, _| {}).unwrap();
        let sha = &tree["test.md"];
        assert_eq!(sha.len(), 40, "should be a full hex SHA");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
