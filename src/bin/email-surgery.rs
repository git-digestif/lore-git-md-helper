//! CLI tool for moving or removing emails in a lore-git-md repository.
//!
//! Handles all artifacts: email `.md`, `.thread.md` (regular file or
//! symlink), AI summaries, thread trees, symlink retargeting, and
//! `refs/notes/msgid` updates.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use lore_git_md_helper::cat_file::CatFile;
use lore_git_md_helper::fast_import::FastImport;
use lore_git_md_helper::msgid_map::hash_message_id;
use lore_git_md_helper::symlink::compute_relative_path;
use lore_git_md_helper::thread_file;

#[derive(Parser)]
#[command(about = "Move or remove emails in a lore-git-md repository")]
struct Cli {
    /// Path to the bare git repository.
    #[arg(long)]
    repo: String,

    /// Git ref to operate on (default: refs/heads/main).
    #[arg(long, default_value = "refs/heads/main")]
    git_ref: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Remove an email (and all its artifacts) from the repository.
    Remove {
        /// Date-key of the email to remove (e.g. 2002/01/02/02-33-05).
        dk: String,
    },
    /// Move an email to a new date-key.
    Move {
        /// Current date-key.
        old_dk: String,
        /// New date-key.
        new_dk: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Remove { dk } => do_remove(&cli.repo, &cli.git_ref, &dk),
        Cmd::Move { old_dk, new_dk } => do_move(&cli.repo, &cli.git_ref, &old_dk, &new_dk),
    }
}

/// Find all files owned by a datekey that exist in the tree.
fn find_owned_files(repo: &str, git_ref: &str, dk: &str) -> Result<Vec<String>> {
    let dk_dir = dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let stdout = lore_git_md_helper::git_util::git(
        repo,
        &["ls-tree", "--name-only", git_ref, &format!("{dk_dir}/")],
    )?;

    let prefix = format!("{dk}.");
    let paths: Vec<String> = stdout
        .lines()
        .filter(|line| {
            // Match files that start with "{dk}." but NOT "{dk}-" (collision suffixes
            // like 23-00-00-1 would wrongly match 23-00-00).
            line.starts_with(&prefix)
        })
        .map(|s| s.to_string())
        .collect();

    Ok(paths)
}

/// Extract the Message-ID from an email's `.md` content.
fn extract_message_id(md_content: &str) -> Option<String> {
    for line in md_content.lines() {
        if !line.contains("**Message-ID**") {
            continue;
        }
        // Format: | **Message-ID** | [<msgid>](url) |
        let start = line.find("| [")?;
        let after_bracket = &line[start + 3..];
        let end = after_bracket.find(']')?;
        return Some(after_bracket[..end].to_string());
    }
    None
}

/// Compute the 3-level fanout path for a Message-ID's note entry.
fn note_path_for(message_id: &str) -> String {
    let oid = hash_message_id(message_id);
    let (d1, rest) = oid.split_at(2);
    let (d2, d3) = rest.split_at(2);
    format!("{d1}/{d2}/{d3}")
}

/// Check whether a `.thread.md` entry is a symlink (mode 120000).
fn is_symlink(repo: &str, git_ref: &str, path: &str) -> Result<bool> {
    let line = lore_git_md_helper::git_util::git(repo, &["ls-tree", git_ref, "--", path])?;
    Ok(line.starts_with("120000"))
}

fn do_remove(repo: &str, git_ref: &str, dk: &str) -> Result<()> {
    let mut cat = CatFile::new(repo).context("failed to start cat-file")?;

    // Load thread tree
    let (root_dk, mut tree) = thread_file::load_from_repo(&mut cat, git_ref, dk)
        .context(format!("no .thread.md found for {dk}"))?;

    let is_root = root_dk == dk;
    let owned = find_owned_files(repo, git_ref, dk)?;
    if owned.is_empty() {
        bail!("no files found for datekey {dk}");
    }

    // Extract Message-ID for notes cleanup
    let md_spec = format!("{git_ref}:{dk}.md");
    let md_content = cat
        .get_str(&md_spec)
        .context(format!("could not read {dk}.md"))?;
    let message_id = extract_message_id(&md_content);

    // Collect all files to delete and files to update
    let mut deletes: Vec<String> = owned.clone();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut symlinks: Vec<(String, String)> = Vec::new();

    let children: Vec<String> = tree.children_of(dk).to_vec();

    if is_root && children.is_empty() {
        // Standalone root with no replies: just delete everything
        eprintln!("Removing standalone thread root {dk}");
    } else if is_root {
        // Root with replies: first child becomes new root
        let new_root = &children[0];
        eprintln!("Re-rooting thread from {dk} to {new_root}");

        tree.remove(dk);

        // The remove left all children as separate roots.  Re-parent
        // the siblings under the new root so the tree stays
        // single-rooted.
        for sibling in &children[1..] {
            tree.reparent(sibling, new_root);
        }

        // The new root's .thread.md needs to become a regular file
        // (it was a symlink pointing to the old root). Delete the
        // symlink first, then emit the regular file.
        let new_root_thread = format!("{new_root}.thread.md");
        deletes.push(new_root_thread.clone());

        let rendered = tree.render(new_root);
        files.push((new_root_thread, rendered));

        // All other thread members that had symlinks pointing to the
        // old root need their symlinks retargeted to the new root.
        let new_root_thread_path = format!("{new_root}.thread.md");
        for member_dk in tree.date_keys() {
            if member_dk == new_root.as_str() {
                continue;
            }
            let member_thread = format!("{member_dk}.thread.md");
            if is_symlink(repo, git_ref, &member_thread)?
                || children.contains(&member_dk.to_string())
            {
                let member_dir = member_dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let target = compute_relative_path(member_dir, &new_root_thread_path);
                symlinks.push((member_thread, target));
            }
        }
    } else {
        // Reply: just remove from thread tree, re-parent children.
        // Children's symlinks still point to root, so no retargeting needed.
        eprintln!("Removing reply {dk} from thread rooted at {root_dk}");
        tree.remove(dk);

        let root_thread = format!("{root_dk}.thread.md");
        let rendered = tree.render(&root_dk);
        files.push((root_thread, rendered));
    }

    // Commit via fast-import
    let tip = resolve_ref(repo, git_ref)?;
    let mut fi = FastImport::new(repo, git_ref)?;
    fi.set_parent(tip);

    let msg = format!("email-surgery: remove {dk}");
    let files_ref: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let symlinks_ref: Vec<(&str, &str)> = symlinks
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let deletes_ref: Vec<&str> = deletes.iter().map(|s| s.as_str()).collect();
    fi.commit_with_symlinks(&msg, &files_ref, &symlinks_ref, &deletes_ref)?;

    // Delete the note entry for this Message-ID
    if let Some(ref msgid) = message_id {
        let mut notes = fi.sibling("refs/notes/msgid");
        if let Ok(tip) = resolve_ref(repo, "refs/notes/msgid") {
            notes.set_parent(tip);
        }
        let path = note_path_for(msgid);
        notes.commit_with_symlinks(
            &format!("email-surgery: remove note for {dk}"),
            &[],
            &[],
            &[path.as_str()],
        )?;
    }

    fi.finish()?;
    eprintln!("Done. Deleted {} files.", deletes.len());
    Ok(())
}

fn do_move(repo: &str, git_ref: &str, old_dk: &str, new_dk: &str) -> Result<()> {
    if old_dk == new_dk {
        bail!("source and target datekey are the same: {old_dk}");
    }

    let mut cat = CatFile::new(repo).context("failed to start cat-file")?;

    // Resolve the target datekey, appending -1, -2, … if it collides
    // with an existing email (same logic as datekey::resolve_key).
    let new_dk = {
        let spec = format!("{git_ref}:{new_dk}.md");
        if cat.get_str(&spec).is_none() {
            new_dk.to_string()
        } else {
            let mut resolved = None;
            for i in 1u32.. {
                let candidate = format!("{new_dk}-{i}");
                let spec = format!("{git_ref}:{candidate}.md");
                if cat.get_str(&spec).is_none() {
                    resolved = Some(candidate);
                    break;
                }
            }
            let dk = resolved.unwrap();
            eprintln!("Target {new_dk} already exists, using {dk}");
            dk
        }
    };

    // Load thread tree
    let (root_dk, mut tree) = thread_file::load_from_repo(&mut cat, git_ref, old_dk)
        .context(format!("no .thread.md found for {old_dk}"))?;

    let is_root = root_dk == old_dk;
    let owned = find_owned_files(repo, git_ref, old_dk)?;
    if owned.is_empty() {
        bail!("no files found for datekey {old_dk}");
    }

    // Extract Message-ID for notes update
    let md_spec = format!("{git_ref}:{old_dk}.md");
    let md_content = cat
        .get_str(&md_spec)
        .context(format!("could not read {old_dk}.md"))?;
    let message_id = extract_message_id(&md_content);

    let mut deletes: Vec<String> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut symlinks: Vec<(String, String)> = Vec::new();

    // Copy all owned files to the new location
    for old_path in &owned {
        let suffix = old_path
            .strip_prefix(old_dk)
            .context(format!("{old_path} doesn't start with {old_dk}"))?;
        let new_path = format!("{new_dk}{suffix}");

        deletes.push(old_path.clone());

        if suffix == ".thread.md" {
            continue; // handled below
        }

        let spec = format!("{git_ref}:{old_path}");
        if let Some(content) = cat.get_str(&spec) {
            // Update only the structured thread-link line, not
            // arbitrary body text that might happen to contain the
            // old datekey.
            let content = if suffix == ".md" {
                content
                    .lines()
                    .map(|line| {
                        if line.starts_with("**Thread**: ") {
                            line.replace(old_dk, &new_dk)
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n"
            } else {
                content
            };
            files.push((new_path, content));
        }
    }

    // Rename in the thread tree
    tree.rename(old_dk, &new_dk);

    let new_root_dk = if is_root {
        new_dk.to_string()
    } else {
        root_dk.clone()
    };

    // Re-render the root's thread file (it references the moved dk)
    let root_thread_path = format!("{new_root_dk}.thread.md");
    let rendered = tree.render(&new_root_dk);
    files.push((root_thread_path.clone(), rendered));

    if is_root {
        // Root moved: all replies need their symlinks retargeted
        for member_dk in tree.date_keys() {
            if member_dk == new_dk {
                continue;
            }
            let member_thread = format!("{member_dk}.thread.md");
            let member_dir = member_dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let target = compute_relative_path(member_dir, &root_thread_path);
            symlinks.push((member_thread, target));
        }
    } else {
        // Reply moved: update this email's .thread.md symlink
        let new_thread = format!("{new_dk}.thread.md");
        let new_dir = new_dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let target = compute_relative_path(new_dir, &root_thread_path);
        symlinks.push((new_thread, target));
    }

    // Commit via fast-import
    let tip = resolve_ref(repo, git_ref)?;
    let mut fi = FastImport::new(repo, git_ref)?;
    fi.set_parent(tip);

    let msg = format!("email-surgery: move {old_dk} to {new_dk}");
    let files_ref: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let symlinks_ref: Vec<(&str, &str)> = symlinks
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let deletes_ref: Vec<&str> = deletes.iter().map(|s| s.as_str()).collect();
    fi.commit_with_symlinks(&msg, &files_ref, &symlinks_ref, &deletes_ref)?;

    // Update the note: same path (same Message-ID hash), new content
    if let Some(ref msgid) = message_id {
        let mut notes = fi.sibling("refs/notes/msgid");
        if let Ok(tip) = resolve_ref(repo, "refs/notes/msgid") {
            notes.set_parent(tip);
        }
        let path = note_path_for(msgid);
        // Note value is stored without trailing newline, matching
        // the format used by emit_notes_update / format_note_value.
        notes.commit(
            &format!("email-surgery: update note for {old_dk} to {new_dk}"),
            &[(&path, new_dk.as_str())],
        )?;
    }

    fi.finish()?;
    eprintln!("Done. Moved {old_dk} to {new_dk}.");
    Ok(())
}

fn resolve_ref(repo: &str, refname: &str) -> Result<String> {
    lore_git_md_helper::git_util::resolve_ref(repo, refname)
        .ok_or_else(|| anyhow::anyhow!("could not resolve {refname}"))
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use lore_git_md_helper::git_util::tests::init_bare_repo;

    /// Generate a minimal email `.md` with a Message-ID in the header table.
    fn email_md(subject: &str, from: &str, msgid: &str, dk: &str) -> String {
        format!(
            "# {subject}\n\
             \n\
             | Header | Value |\n\
             |--------|-------|\n\
             | **From** | {from} |\n\
             | **Date** | 2025-01-01T00:00:00+00:00 |\n\
             | **Message-ID** | [{msgid}](https://lore.kernel.org/git/{msgid}) |\n\
             \n\
             ---\n\
             \n\
             **Thread**: [thread](./{ts}.thread.md#:~:text={dk})\n\
             \n\
             Some email body.\n",
            ts = dk.rsplit_once('/').unwrap().1,
        )
    }

    /// Check whether a path exists in the tree.
    fn file_exists(repo: &str, git_ref: &str, path: &str) -> bool {
        lore_git_md_helper::git_util::git(repo, &["ls-tree", git_ref, "--", path])
            .is_ok_and(|out| !out.is_empty())
    }

    /// Read a file's content from the tree.
    fn read_file(repo: &str, git_ref: &str, path: &str) -> Option<String> {
        let mut cat = CatFile::new(repo).ok()?;
        cat.get_str(&format!("{git_ref}:{path}"))
    }

    /// Seed a test repo with a thread: root + optional replies.
    /// Returns the repo path as a String.
    fn seed_repo(
        dir: &tempfile::TempDir,
        emails: &[(&str, &str, &str, &str)], // (dk, subject, from, msgid)
        reply_parents: &[(&str, &str)],      // (reply_dk, parent_dk) for tree structure
    ) -> String {
        let repo = dir.path().to_str().unwrap().to_string();
        let mut fi = FastImport::new(&repo, "refs/heads/main").unwrap();

        // Build the thread tree
        let mut tree = lore_git_md_helper::thread_file::ThreadTree::new();
        for (dk, subject, from, _msgid) in emails {
            let parent = reply_parents
                .iter()
                .find(|(child, _)| child == dk)
                .map(|(_, p)| *p);
            tree.insert(dk, parent, subject, from);
        }

        let root_dk = emails[0].0;
        let root_thread_path = format!("{root_dk}.thread.md");

        // Build files list
        let mut files: Vec<(String, String)> = Vec::new();
        let mut symlinks: Vec<(String, String)> = Vec::new();

        for (dk, subject, from, msgid) in emails {
            // Email .md
            files.push((format!("{dk}.md"), email_md(subject, from, msgid, dk)));

            if *dk == root_dk {
                // Root: .thread.md is a regular file
                files.push((root_thread_path.clone(), tree.render(root_dk)));
            } else {
                // Reply: .thread.md is a symlink to root's .thread.md
                let dk_dir = dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let target = compute_relative_path(dk_dir, &root_thread_path);
                symlinks.push((format!("{dk}.thread.md"), target));
            }
        }

        let files_ref: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let symlinks_ref: Vec<(&str, &str)> = symlinks
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        fi.commit_with_symlinks("seed", &files_ref, &symlinks_ref, &[])
            .unwrap();

        // Also seed notes
        let mut notes = fi.sibling("refs/notes/msgid");
        let mut note_files: Vec<(String, String)> = Vec::new();
        for (dk, _subject, _from, msgid) in emails {
            let path = note_path_for(msgid);
            note_files.push((path, dk.to_string()));
        }
        let note_files_ref: Vec<(&str, &str)> = note_files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        notes.commit("seed notes", &note_files_ref).unwrap();

        fi.finish().unwrap();
        repo
    }

    #[test]
    fn test_remove_standalone_root() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[(
                "2002/01/02/02-33-05",
                "Spam subject",
                "Spammer",
                "spam@example.com",
            )],
            &[],
        );

        // Verify setup
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2002/01/02/02-33-05.md"
        ));
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2002/01/02/02-33-05.thread.md"
        ));

        // Remove it
        do_remove(&repo, "refs/heads/main", "2002/01/02/02-33-05").unwrap();

        // Verify all files gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2002/01/02/02-33-05.md"
        ));
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2002/01/02/02-33-05.thread.md"
        ));

        // Verify note deleted
        let note_path = note_path_for("spam@example.com");
        assert!(!file_exists(&repo, "refs/notes/msgid", &note_path));
    }

    #[test]
    fn test_remove_reply() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                (
                    "2025/01/01/10-00-00",
                    "Root email",
                    "Alice",
                    "root@example.com",
                ),
                (
                    "2025/01/01/11-00-00",
                    "Re: Root email",
                    "Bob",
                    "reply@example.com",
                ),
                (
                    "2025/01/01/12-00-00",
                    "Re: Re: Root email",
                    "Carol",
                    "reply2@example.com",
                ),
            ],
            &[
                ("2025/01/01/11-00-00", "2025/01/01/10-00-00"),
                ("2025/01/01/12-00-00", "2025/01/01/11-00-00"),
            ],
        );

        // Remove the middle reply; Carol should be re-parented to Alice
        do_remove(&repo, "refs/heads/main", "2025/01/01/11-00-00").unwrap();

        // Reply files gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2025/01/01/11-00-00.md"
        ));
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2025/01/01/11-00-00.thread.md"
        ));

        // Root and Carol still exist
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2025/01/01/10-00-00.md"
        ));
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2025/01/01/12-00-00.md"
        ));

        // Thread file updated: Carol is now a direct child of root
        let thread = read_file(&repo, "refs/heads/main", "2025/01/01/10-00-00.thread.md").unwrap();
        assert!(
            !thread.contains("2025/01/01/11-00-00"),
            "removed reply should not appear in thread"
        );
        assert!(
            thread.contains("2025/01/01/12-00-00"),
            "Carol should still be in thread"
        );
        // Carol should be at depth 1 (direct child of root), not depth 2
        for line in thread.lines() {
            if line.contains("2025/01/01/12-00-00") {
                assert!(
                    line.starts_with("  - "),
                    "Carol should be at depth 1, got: {line}"
                );
            }
        }
    }

    #[test]
    fn test_remove_root_reroots_to_first_child() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                (
                    "1999/12/10/23-00-00",
                    "Original root",
                    "Andreas",
                    "root@op5.se",
                ),
                (
                    "2006/01/06/22-37-48",
                    "Re: Original root",
                    "Junio",
                    "reply@git.org",
                ),
            ],
            &[("2006/01/06/22-37-48", "1999/12/10/23-00-00")],
        );

        // Remove the root
        do_remove(&repo, "refs/heads/main", "1999/12/10/23-00-00").unwrap();

        // Old root files gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "1999/12/10/23-00-00.md"
        ));
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "1999/12/10/23-00-00.thread.md"
        ));

        // New root exists with a regular .thread.md file
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/06/22-37-48.md"
        ));
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/06/22-37-48.thread.md"
        ));

        let thread = read_file(&repo, "refs/heads/main", "2006/01/06/22-37-48.thread.md").unwrap();
        assert!(
            thread.contains("2006/01/06/22-37-48"),
            "new root should be in thread"
        );
        assert!(
            !thread.contains("1999/12/10/23-00-00"),
            "old root should not be in thread"
        );
    }

    #[test]
    fn test_move_reply() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                (
                    "2007/10/22/06-11-15",
                    "Thread root",
                    "Alice",
                    "root@example.com",
                ),
                (
                    "2029/03/30/12-56-20",
                    "Re: Thread root",
                    "Bob",
                    "reply@example.com",
                ),
            ],
            &[("2029/03/30/12-56-20", "2007/10/22/06-11-15")],
        );

        // Move the misdated reply to the correct date
        do_move(
            &repo,
            "refs/heads/main",
            "2029/03/30/12-56-20",
            "2007/12/11/12-56-20",
        )
        .unwrap();

        // Old location gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2029/03/30/12-56-20.md"
        ));
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2029/03/30/12-56-20.thread.md"
        ));

        // New location exists
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2007/12/11/12-56-20.md"
        ));
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2007/12/11/12-56-20.thread.md"
        ));

        // Thread file updated with new datekey
        let thread = read_file(&repo, "refs/heads/main", "2007/10/22/06-11-15.thread.md").unwrap();
        assert!(
            !thread.contains("2029/03/30/12-56-20"),
            "old dk should not be in thread"
        );
        assert!(
            thread.contains("2007/12/11/12-56-20"),
            "new dk should be in thread"
        );

        // Note updated
        let note_path = note_path_for("reply@example.com");
        let note = read_file(&repo, "refs/notes/msgid", &note_path).unwrap();
        assert_eq!(note, "2007/12/11/12-56-20", "note should point to new dk");

        // Email .md content updated (self-references replaced)
        let md = read_file(&repo, "refs/heads/main", "2007/12/11/12-56-20.md").unwrap();
        assert!(
            !md.contains("2029/03/30/12-56-20"),
            "old dk should not appear in email content"
        );
        assert!(
            md.contains("2007/12/11/12-56-20"),
            "new dk should appear in email content"
        );
    }

    #[test]
    fn test_move_root_retargets_reply_symlinks() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                (
                    "1999/12/10/23-00-00",
                    "Misdated root",
                    "Andreas",
                    "root@op5.se",
                ),
                (
                    "2006/01/06/22-37-48",
                    "Re: Misdated root",
                    "Junio",
                    "reply@git.org",
                ),
            ],
            &[("2006/01/06/22-37-48", "1999/12/10/23-00-00")],
        );

        // Move the misdated root to the correct date
        do_move(
            &repo,
            "refs/heads/main",
            "1999/12/10/23-00-00",
            "2006/01/04/16-17-29",
        )
        .unwrap();

        // Old root gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "1999/12/10/23-00-00.md"
        ));

        // New root exists
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/04/16-17-29.md"
        ));
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/04/16-17-29.thread.md"
        ));

        // Thread file at the new root location references both emails
        let thread = read_file(&repo, "refs/heads/main", "2006/01/04/16-17-29.thread.md").unwrap();
        assert!(
            thread.contains("2006/01/04/16-17-29"),
            "new root dk in thread"
        );
        assert!(thread.contains("2006/01/06/22-37-48"), "reply dk in thread");

        // Reply's symlink retargeted: reading via the reply should give
        // the same thread content (CatFile follows symlinks)
        let mut cat = CatFile::new(&repo).unwrap();
        let reply_thread = cat.get_str("refs/heads/main:2006/01/06/22-37-48.thread.md");
        assert!(reply_thread.is_some(), "reply .thread.md should resolve");
        assert_eq!(
            reply_thread.unwrap(),
            thread,
            "reply .thread.md should point to same content as root"
        );
    }

    #[test]
    fn test_extract_message_id() {
        let md = "# Subject\n\
            \n\
            | Header | Value |\n\
            |--------|-------|\n\
            | **From** | Alice |\n\
            | **Message-ID** | [test@example.com](https://lore.kernel.org/git/test@example.com) |\n";
        assert_eq!(extract_message_id(md), Some("test@example.com".to_string()));
    }

    #[test]
    fn test_extract_message_id_missing() {
        let md = "# Subject\n\nJust some text.\n";
        assert_eq!(extract_message_id(md), None);
    }

    #[test]
    fn test_remove_root_with_multiple_children_preserves_all() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                ("2020/01/01/00-00-00", "Root", "Alice", "root@example.com"),
                ("2020/01/02/00-00-00", "Reply A", "Bob", "a@example.com"),
                ("2020/01/03/00-00-00", "Reply B", "Carol", "b@example.com"),
                ("2020/01/04/00-00-00", "Reply C", "Dave", "c@example.com"),
            ],
            &[
                ("2020/01/02/00-00-00", "2020/01/01/00-00-00"),
                ("2020/01/03/00-00-00", "2020/01/01/00-00-00"),
                ("2020/01/04/00-00-00", "2020/01/01/00-00-00"),
            ],
        );

        do_remove(&repo, "refs/heads/main", "2020/01/01/00-00-00").unwrap();

        // Old root gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2020/01/01/00-00-00.md"
        ));

        // First child is the new root; siblings are re-parented under it
        let thread = read_file(&repo, "refs/heads/main", "2020/01/02/00-00-00.thread.md").unwrap();
        assert_eq!(
            thread,
            concat!(
                "# Thread\n",
                "\n",
                "- 2020/01/02/00-00-00 [Reply A](00-00-00.md) *Bob*\n",
                "  - 2020/01/03/00-00-00 [Reply B](../03/00-00-00.md) *Carol*\n",
                "  - 2020/01/04/00-00-00 [Reply C](../04/00-00-00.md) *Dave*\n",
            ),
        );
    }

    #[test]
    fn test_remove_non_root_with_multiple_children() {
        let dir = init_bare_repo();
        let repo = seed_repo(
            &dir,
            &[
                ("2020/02/01/00-00-00", "Root", "Alice", "root2@example.com"),
                ("2020/02/02/00-00-00", "Middle", "Bob", "mid@example.com"),
                ("2020/02/03/00-00-00", "Child A", "Carol", "ca@example.com"),
                ("2020/02/04/00-00-00", "Child B", "Dave", "cb@example.com"),
            ],
            &[
                ("2020/02/02/00-00-00", "2020/02/01/00-00-00"),
                ("2020/02/03/00-00-00", "2020/02/02/00-00-00"),
                ("2020/02/04/00-00-00", "2020/02/02/00-00-00"),
            ],
        );

        do_remove(&repo, "refs/heads/main", "2020/02/02/00-00-00").unwrap();

        // Middle node gone
        assert!(!file_exists(
            &repo,
            "refs/heads/main",
            "2020/02/02/00-00-00.md"
        ));

        // Children re-parented under root
        let thread = read_file(&repo, "refs/heads/main", "2020/02/01/00-00-00.thread.md").unwrap();
        assert_eq!(
            thread,
            concat!(
                "# Thread\n",
                "\n",
                "- 2020/02/01/00-00-00 [Root](00-00-00.md) *Alice*\n",
                "  - 2020/02/03/00-00-00 [Child A](../03/00-00-00.md) *Carol*\n",
                "  - 2020/02/04/00-00-00 [Child B](../04/00-00-00.md) *Dave*\n",
            ),
        );
    }

    #[test]
    fn test_move_collision_avoidance() {
        let dir = init_bare_repo();
        // Seed two standalone emails: one at the target datekey, one to move
        let repo = dir.path().to_str().unwrap().to_string();
        let mut fi = FastImport::new(&repo, "refs/heads/main").unwrap();

        let mut tree1 = lore_git_md_helper::thread_file::ThreadTree::new();
        tree1.insert("2006/01/04/16-17-29", None, "Existing email", "Alice");

        let mut tree2 = lore_git_md_helper::thread_file::ThreadTree::new();
        tree2.insert("1999/12/10/23-00-00", None, "Misdated email", "Bob");

        fi.commit_with_symlinks(
            "seed",
            &[
                (
                    "2006/01/04/16-17-29.md",
                    &email_md(
                        "Existing email",
                        "Alice",
                        "existing@example.com",
                        "2006/01/04/16-17-29",
                    ),
                ),
                (
                    "2006/01/04/16-17-29.thread.md",
                    &tree1.render("2006/01/04/16-17-29"),
                ),
                (
                    "1999/12/10/23-00-00.md",
                    &email_md(
                        "Misdated email",
                        "Bob",
                        "misdated@example.com",
                        "1999/12/10/23-00-00",
                    ),
                ),
                (
                    "1999/12/10/23-00-00.thread.md",
                    &tree2.render("1999/12/10/23-00-00"),
                ),
            ],
            &[],
            &[],
        )
        .unwrap();

        let mut notes = fi.sibling("refs/notes/msgid");
        notes
            .commit(
                "seed notes",
                &[
                    (
                        &note_path_for("existing@example.com"),
                        "2006/01/04/16-17-29",
                    ),
                    (
                        &note_path_for("misdated@example.com"),
                        "1999/12/10/23-00-00",
                    ),
                ],
            )
            .unwrap();

        fi.finish().unwrap();

        // Move the misdated email to a datekey that is already taken
        do_move(
            &repo,
            "refs/heads/main",
            "1999/12/10/23-00-00",
            "2006/01/04/16-17-29",
        )
        .unwrap();

        // The original email at the target datekey should be untouched
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/04/16-17-29.md"
        ));
        let existing = read_file(&repo, "refs/heads/main", "2006/01/04/16-17-29.md").unwrap();
        assert!(
            existing.contains("Existing email"),
            "original email should be untouched"
        );

        // The moved email should land at the -1 suffix
        assert!(file_exists(
            &repo,
            "refs/heads/main",
            "2006/01/04/16-17-29-1.md"
        ));
        let moved = read_file(&repo, "refs/heads/main", "2006/01/04/16-17-29-1.md").unwrap();
        assert!(moved.contains("Misdated email"), "moved email at -1 suffix");

        // Note updated to the -1 suffix
        let note_path = note_path_for("misdated@example.com");
        let note = read_file(&repo, "refs/notes/msgid", &note_path).unwrap();
        assert_eq!(note, "2006/01/04/16-17-29-1");
    }
}
