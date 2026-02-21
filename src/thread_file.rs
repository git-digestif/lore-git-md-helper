//! Generate `.thread.md` files showing thread structure.
//!
//! Modeled after the "Thread overview" section on lore.kernel.org, but
//! rendered as Markdown with relative links to the email files in the
//! same repository.

use std::collections::HashMap;

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

    /// Check whether a date-key is in the tree.
    pub fn contains(&self, date_key: &str) -> bool {
        self.nodes.contains_key(date_key)
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
}

/// Escape characters that would break markdown link text.
fn escape_markdown_link_text(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
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
}
