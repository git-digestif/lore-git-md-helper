//! Generate and parse `.thread.md` files showing thread structure.
//!
//! Modeled after the "Thread overview" section on lore.kernel.org, but
//! rendered as Markdown with relative links to the email files in the
//! same repository.
//!
//! The tree supports incremental construction: emails may arrive out of
//! order, and re-parenting is supported when a previously-missing parent
//! email finally shows up.

use std::collections::HashMap;

use crate::cat_file::BlobRead;

/// Metadata for one email in the thread.
#[derive(Debug, Clone)]
pub struct ThreadNode {
    pub date_key: String,
    pub subject: String,
    pub from: String,
}

/// A thread tree with parent-child relationships.
///
/// Nodes are keyed by date-key. Each node has an optional parent
/// (None = root of the thread). Children are ordered by insertion time.
#[derive(Debug, Clone)]
pub struct ThreadTree {
    nodes: HashMap<String, ThreadNode>,
    /// parent_date_key for each node (None = root).
    parents: HashMap<String, Option<String>>,
    /// Children of each node, in insertion order.
    children: HashMap<String, Vec<String>>,
    /// Root nodes (in insertion order).
    roots: Vec<String>,
}

impl Default for ThreadTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadTree {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            parents: HashMap::new(),
            children: HashMap::new(),
            roots: Vec::new(),
        }
    }

    /// Insert a node into the tree.
    ///
    /// `parent_date_key` is the date-key of the parent node, or `None`
    /// if this is a root. If the parent isn't in the tree, the node
    /// becomes a root (caller should handle this case).
    pub fn insert(
        &mut self,
        date_key: &str,
        parent_date_key: Option<&str>,
        subject: &str,
        from: &str,
    ) {
        assert!(
            !self.nodes.contains_key(date_key),
            "duplicate date-key in thread: {date_key}"
        );

        self.nodes.insert(
            date_key.to_string(),
            ThreadNode {
                date_key: date_key.to_string(),
                subject: subject.to_string(),
                from: from.to_string(),
            },
        );

        let actual_parent = match parent_date_key {
            Some(pk) if self.nodes.contains_key(pk) => Some(pk.to_string()),
            _ => None,
        };

        self.parents
            .insert(date_key.to_string(), actual_parent.clone());

        if let Some(ref pk) = actual_parent {
            self.children
                .entry(pk.clone())
                .or_default()
                .push(date_key.to_string());
        } else {
            self.roots.push(date_key.to_string());
        }

        self.children.entry(date_key.to_string()).or_default();
    }

    /// Move a node (and its subtree) to a new parent.
    ///
    /// Used when a previously-missing email arrives and its children
    /// need to be re-parented under it.
    pub fn reparent(&mut self, child_date_key: &str, new_parent_date_key: &str) {
        assert!(
            self.nodes.contains_key(child_date_key),
            "reparent: child {child_date_key} not in tree"
        );
        assert!(
            self.nodes.contains_key(new_parent_date_key),
            "reparent: new parent {new_parent_date_key} not in tree"
        );

        // Remove from old parent's children list (or roots)
        let old_parent = self.parents.get(child_date_key).cloned().flatten();
        if let Some(ref old_pk) = old_parent {
            if let Some(siblings) = self.children.get_mut(old_pk) {
                siblings.retain(|dk| dk != child_date_key);
            }
        } else {
            self.roots.retain(|dk| dk != child_date_key);
        }

        // Add to new parent's children
        self.children
            .entry(new_parent_date_key.to_string())
            .or_default()
            .push(child_date_key.to_string());

        self.parents.insert(
            child_date_key.to_string(),
            Some(new_parent_date_key.to_string()),
        );
    }

    /// Remove a node from the tree, re-parenting its children to the
    /// removed node's parent.
    pub fn remove(&mut self, dk: &str) {
        let parent = self.parents.remove(dk).flatten();
        let children = self.children.remove(dk).unwrap_or_default();
        self.nodes.remove(dk);

        // Remove from parent's children list (or roots)
        if let Some(ref p) = parent {
            if let Some(siblings) = self.children.get_mut(p) {
                siblings.retain(|c| c != dk);
            }
        } else {
            self.roots.retain(|r| r != dk);
        }

        // Re-parent children
        for child in &children {
            self.parents.insert(child.clone(), parent.clone());
            if let Some(ref p) = parent {
                self.children
                    .entry(p.clone())
                    .or_default()
                    .push(child.clone());
            } else {
                self.roots.push(child.clone());
            }
        }
    }

    /// Change a node's date-key, updating all internal references.
    pub fn rename(&mut self, old_dk: &str, new_dk: &str) {
        if let Some(mut node) = self.nodes.remove(old_dk) {
            node.date_key = new_dk.to_string();
            self.nodes.insert(new_dk.to_string(), node);
        }

        if let Some(parent) = self.parents.remove(old_dk) {
            self.parents.insert(new_dk.to_string(), parent.clone());
            if let Some(ref p) = parent {
                if let Some(siblings) = self.children.get_mut(p) {
                    for s in siblings.iter_mut() {
                        if s == old_dk {
                            *s = new_dk.to_string();
                        }
                    }
                }
            } else {
                for r in self.roots.iter_mut() {
                    if r == old_dk {
                        *r = new_dk.to_string();
                    }
                }
            }
        }

        if let Some(children) = self.children.remove(old_dk) {
            for child in &children {
                if let Some(p) = self.parents.get_mut(child) {
                    *p = Some(new_dk.to_string());
                }
            }
            self.children.insert(new_dk.to_string(), children);
        }
    }

    /// Check whether a date-key is in the tree.
    pub fn contains(&self, date_key: &str) -> bool {
        self.nodes.contains_key(date_key)
    }

    /// Get the children of a node (empty slice if none).
    pub fn children_of(&self, date_key: &str) -> &[String] {
        self.children
            .get(date_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get all date-keys in the tree.
    pub fn date_keys(&self) -> impl Iterator<Item = &str> {
        self.nodes.keys().map(|s| s.as_str())
    }

    /// Return the date-key of a node's parent (`None` for root nodes
    /// or if the date-key is not in the tree).
    pub fn parent_of(&self, date_key: &str) -> Option<&str> {
        self.parents.get(date_key)?.as_deref()
    }

    /// Return the date-key of the first root node (depth-0, no parent).
    ///
    /// Well-formed thread files always have exactly one root, but this
    /// gracefully handles multi-root trees by returning the first one.
    pub fn first_root(&self) -> Option<&str> {
        self.roots.first().map(|s| s.as_str())
    }

    /// Render the thread tree as a `.thread.md` file.
    ///
    /// `root_dk` is the date-key of the thread root, used to compute
    /// relative link paths (the thread file lives at `<root_dk>.thread.md`).
    pub fn render(&self, root_dk: &str) -> String {
        let root_dir = root_dk.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let mut md = String::from("# Thread\n\n");
        for root in &self.roots {
            self.render_subtree(&mut md, root, 0, root_dir);
        }
        md
    }

    fn render_subtree(&self, md: &mut String, date_key: &str, depth: usize, root_dir: &str) {
        if let Some(node) = self.nodes.get(date_key) {
            let indent = "  ".repeat(depth);
            let link_target =
                crate::symlink::compute_relative_path(root_dir, &format!("{}.md", node.date_key));
            let link = format!(
                "[{subject}]({link_target})",
                subject = escape_markdown_link_text(&node.subject),
            );
            md.push_str(&format!(
                "{indent}- {dk} {link} *{from}*\n",
                dk = node.date_key,
                from = node.from,
            ));
        }
        if let Some(children) = self.children.get(date_key) {
            for child in children {
                self.render_subtree(md, child, depth + 1, root_dir);
            }
        }
    }

    /// Parse a `.thread.md` file back into a tree.
    ///
    /// Reconstructs parent-child relationships from indentation:
    /// an entry at depth N is a child of the most recent entry at
    /// depth N-1 above it.
    pub fn parse(md: &str) -> Self {
        let mut tree = Self::new();
        // Stack of (date_key, depth) for tracking parentage
        let mut stack: Vec<(String, usize)> = Vec::new();

        for line in md.lines() {
            if !line.contains("- ") {
                continue;
            }

            let trimmed = line.trim_start();
            let leading_spaces = line.len() - trimmed.len();
            let depth = leading_spaces / 2;

            let rest = match trimmed.strip_prefix("- ") {
                Some(r) => r,
                None => continue,
            };

            let (date_key, rest) = match rest.split_once(' ') {
                Some(pair) => pair,
                None => continue,
            };

            let subject = match rest.find("](") {
                Some(bracket_end) => {
                    if !rest.starts_with('[') {
                        continue;
                    }
                    unescape_markdown_link_text(&rest[1..bracket_end])
                }
                None => continue,
            };

            let from = match rest.rfind('*') {
                Some(last_star) => {
                    let before_last = &rest[..last_star];
                    match before_last.rfind('*') {
                        Some(first_star) => rest[first_star + 1..last_star].to_string(),
                        None => continue,
                    }
                }
                None => continue,
            };

            // Pop stack to find parent at depth-1
            while stack.last().is_some_and(|(_, d)| *d >= depth) {
                stack.pop();
            }

            let parent = stack.last().map(|(dk, _)| dk.as_str());
            tree.insert(date_key, parent, &subject, &from);

            stack.push((date_key.to_string(), depth));
        }

        tree
    }
}

/// Escape characters that would break markdown link text.
fn escape_markdown_link_text(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
}

/// Unescape markdown link text back to plain text.
fn unescape_markdown_link_text(s: &str) -> String {
    s.replace("\\[", "[").replace("\\]", "]")
}

/// Load a thread tree from a git repository, following symlinks.
///
/// Uses the provided `BlobRead` implementation (typically a persistent
/// `CatFile` process) to transparently resolve symlink chains without
/// spawning a new process per lookup.  Returns the real thread root's
/// date-key (extracted from the first entry) together with the parsed
/// tree.
/// Returns `None` if no `.thread.md` exists for this date-key.
pub fn load_from_repo(
    cat: &mut impl BlobRead,
    git_ref: &str,
    dk: &str,
) -> Option<(String, ThreadTree)> {
    let spec = format!("{git_ref}:{dk}.thread.md");
    let md = cat.get_str(&spec)?;
    let tree = ThreadTree::parse(&md);
    let root_dk = tree.first_root()?.to_string();
    Some((root_dk, tree))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_single_entry() {
        let mut tree = ThreadTree::new();
        tree.insert(
            "2025/02/12/04-10-17",
            None,
            "[PATCH] Fix race condition",
            "Alice",
        );
        assert_eq!(
            tree.render("2025/02/12/04-10-17"),
            concat!(
                "# Thread\n",
                "\n",
                "- 2025/02/12/04-10-17 [\\[PATCH\\] Fix race condition](04-10-17.md) *Alice*\n",
            ),
        );
    }

    #[test]
    fn test_render_nested_thread() {
        let mut tree = ThreadTree::new();
        tree.insert("2025/02/12/04-10-17", None, "[PATCH 0/2] series", "Alice");
        tree.insert(
            "2025/02/12/04-10-18",
            Some("2025/02/12/04-10-17"),
            "[PATCH 1/2] first",
            "Alice",
        );
        tree.insert(
            "2025/02/12/19-30-00",
            Some("2025/02/12/04-10-18"),
            "Re: [PATCH 1/2] first",
            "Bob",
        );
        tree.insert(
            "2025/02/12/04-10-19",
            Some("2025/02/12/04-10-17"),
            "[PATCH 2/2] second",
            "Alice",
        );

        assert_eq!(
            tree.render("2025/02/12/04-10-17"),
            concat!(
                "# Thread\n",
                "\n",
                "- 2025/02/12/04-10-17 [\\[PATCH 0/2\\] series](04-10-17.md) *Alice*\n",
                "  - 2025/02/12/04-10-18 [\\[PATCH 1/2\\] first](04-10-18.md) *Alice*\n",
                "    - 2025/02/12/19-30-00 [Re: \\[PATCH 1/2\\] first](19-30-00.md) *Bob*\n",
                "  - 2025/02/12/04-10-19 [\\[PATCH 2/2\\] second](04-10-19.md) *Alice*\n",
            ),
        );
    }

    #[test]
    fn test_escape_brackets() {
        assert_eq!(escape_markdown_link_text("[PATCH] foo"), "\\[PATCH\\] foo");
    }

    #[test]
    fn test_round_trip() {
        let mut tree = ThreadTree::new();
        tree.insert("2025/02/12/04-10-17", None, "[PATCH 0/2] series", "Alice");
        tree.insert(
            "2025/02/12/04-10-18",
            Some("2025/02/12/04-10-17"),
            "[PATCH 1/2] first",
            "Alice",
        );
        tree.insert(
            "2025/02/12/19-30-00",
            Some("2025/02/12/04-10-18"),
            "Re: [PATCH 1/2] first",
            "Bob",
        );

        let md = tree.render("2025/02/12/04-10-17");
        let parsed = ThreadTree::parse(&md);
        let md2 = parsed.render("2025/02/12/04-10-17");
        assert_eq!(md, md2);
    }

    #[test]
    fn test_parse_empty() {
        let tree = ThreadTree::parse("# Thread\n\n");
        assert_eq!(tree.nodes.len(), 0);
    }

    // ===== Out-of-order arrival: B→A, C→B, D→A, F→E(refs:A,B,C), E arrives =====

    /// Build the base thread:
    /// A is root, B replies to A, C replies to B, D replies to A.
    fn base_thread() -> ThreadTree {
        let mut tree = ThreadTree::new();
        tree.insert("dk-A", None, "subject-A", "Author-A");
        tree.insert("dk-B", Some("dk-A"), "subject-B", "Author-B");
        tree.insert("dk-C", Some("dk-B"), "subject-C", "Author-C");
        tree.insert("dk-D", Some("dk-A"), "subject-D", "Author-D");
        tree
    }

    #[test]
    fn test_ooo_base_structure() {
        let tree = base_thread();
        assert_eq!(
            tree.render("dk-A"),
            concat!(
                "# Thread\n",
                "\n",
                "- dk-A [subject-A](dk-A.md) *Author-A*\n",
                "  - dk-B [subject-B](dk-B.md) *Author-B*\n",
                "    - dk-C [subject-C](dk-C.md) *Author-C*\n",
                "  - dk-D [subject-D](dk-D.md) *Author-D*\n",
            ),
        );
    }

    #[test]
    fn test_ooo_missing_parent() {
        // F replies to E, but E is missing.
        // References for F are [A, B, C, E]. E is not in the tree.
        // Caller walks refs backward: E missing, C found → parent = C.
        let mut tree = base_thread();
        tree.insert("dk-F", Some("dk-C"), "subject-F", "Author-F");

        assert_eq!(
            tree.render("dk-A"),
            concat!(
                "# Thread\n",
                "\n",
                "- dk-A [subject-A](dk-A.md) *Author-A*\n",
                "  - dk-B [subject-B](dk-B.md) *Author-B*\n",
                "    - dk-C [subject-C](dk-C.md) *Author-C*\n",
                "      - dk-F [subject-F](dk-F.md) *Author-F*\n",
                "  - dk-D [subject-D](dk-D.md) *Author-D*\n",
            ),
        );
    }

    #[test]
    fn test_ooo_reparent_when_missing_arrives() {
        // F was placed under C because E was missing.
        // Now E arrives (replies to C). F should be re-parented under E.
        let mut tree = base_thread();
        tree.insert("dk-F", Some("dk-C"), "subject-F", "Author-F");

        // E arrives, replies to C
        tree.insert("dk-E", Some("dk-C"), "subject-E", "Author-E");

        // Re-parent F under E (caller detected F's In-Reply-To was E)
        tree.reparent("dk-F", "dk-E");

        // After reparent: C's children = [dk-E] (dk-F removed)
        // E's children = [dk-F]
        // DFS: A → B → C → E → F, then D
        assert_eq!(
            tree.render("dk-A"),
            concat!(
                "# Thread\n",
                "\n",
                "- dk-A [subject-A](dk-A.md) *Author-A*\n",
                "  - dk-B [subject-B](dk-B.md) *Author-B*\n",
                "    - dk-C [subject-C](dk-C.md) *Author-C*\n",
                "      - dk-E [subject-E](dk-E.md) *Author-E*\n",
                "        - dk-F [subject-F](dk-F.md) *Author-F*\n",
                "  - dk-D [subject-D](dk-D.md) *Author-D*\n",
            ),
        );
    }

    /// Regression test: when an email references not-yet-seen parents, it
    /// gets placed under the closest known ancestor. When the missing parent
    /// arrives and insert_known() returns WantedBy, the caller must reparent.
    ///
    /// Scenario modeled after a real lore-git thread:
    ///   1. Cover letter (A) arrives → root
    ///   2. Review (R) arrives, References: [A, B, C] — B and C unknown.
    ///      R threads under A (closest known root).
    ///   3. B arrives, References: [A] → child of A.
    ///      insert_known("B") returns WantedBy(["R"]) → reparent R under B.
    ///   4. C arrives, References: [A, B] → child of B.
    ///      insert_known("C") returns WantedBy(["R"]) → reparent R under C.
    #[test]
    fn test_ooo_reparent_via_wanted_by() {
        use crate::msgid_map::{MsgIdEntry, MsgIdMap};

        let mut map = MsgIdMap::new(None);
        let mut tree = ThreadTree::new();

        // Step 1: cover letter A
        map.insert_known("a@example.com", "dk-A".into());
        tree.insert("dk-A", None, "cover letter", "Author");

        // Step 2: review R references [A, B, C] — B and C unknown
        let refs = ["a@example.com", "b@example.com", "c@example.com"];
        for r in &refs[1..] {
            map.insert(r, MsgIdEntry::WantedBy(vec!["dk-R".into()]));
        }
        // R threads under A (the only Known ancestor)
        tree.insert("dk-R", Some("dk-A"), "review", "Reviewer");

        // Step 3: B arrives
        let old = map.insert_known("b@example.com", "dk-B".into());
        tree.insert("dk-B", Some("dk-A"), "patch 1", "Author");
        if let Some(MsgIdEntry::WantedBy(waiters)) = old {
            for w in &waiters {
                if tree.contains(w) {
                    tree.reparent(w, "dk-B");
                }
            }
        }

        // Step 4: C arrives
        let old = map.insert_known("c@example.com", "dk-C".into());
        tree.insert("dk-C", Some("dk-B"), "patch 2", "Author");
        if let Some(MsgIdEntry::WantedBy(waiters)) = old {
            for w in &waiters {
                if tree.contains(w) {
                    tree.reparent(w, "dk-C");
                }
            }
        }

        // R should now be under C (its actual In-Reply-To target)
        assert_eq!(
            tree.render("dk-A"),
            concat!(
                "# Thread\n",
                "\n",
                "- dk-A [cover letter](dk-A.md) *Author*\n",
                "  - dk-B [patch 1](dk-B.md) *Author*\n",
                "    - dk-C [patch 2](dk-C.md) *Author*\n",
                "      - dk-R [review](dk-R.md) *Reviewer*\n",
            ),
        );
    }
}
