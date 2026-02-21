//! Compute relative symlink targets for thread files.
//!
//! Given a directory `from_dir` (e.g. `"2025/02/12"`) and a target path
//! `to_path` (e.g. `"2025/02/10/04-10-17.thread.md"`), returns the
//! relative path from the former to the latter using `../` segments.

/// Compute relative path from `from_dir` to `to_path`.
///
/// Both paths use `/` as separator.  `from_dir` is a directory (no
/// trailing slash), `to_path` is a file path.
pub fn compute_relative_path(from_dir: &str, to_path: &str) -> String {
    let from_parts: Vec<&str> = if from_dir.is_empty() {
        vec![]
    } else {
        from_dir.split('/').collect()
    };
    let to_parts: Vec<&str> = to_path.split('/').collect();

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let ups = from_parts.len() - common;
    let mut result = String::new();
    for _ in 0..ups {
        result.push_str("../");
    }
    result.push_str(&to_parts[common..].join("/"));

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_directory() {
        assert_eq!(
            compute_relative_path("2025/02/12", "2025/02/12/04-10-17.thread.md"),
            "04-10-17.thread.md",
        );
    }

    #[test]
    fn test_sibling_directory() {
        assert_eq!(
            compute_relative_path("2025/02/13", "2025/02/12/04-10-17.thread.md"),
            "../12/04-10-17.thread.md",
        );
    }

    #[test]
    fn test_different_month() {
        assert_eq!(
            compute_relative_path("2025/03/01", "2025/02/12/04-10-17.thread.md"),
            "../../02/12/04-10-17.thread.md",
        );
    }

    #[test]
    fn test_different_year() {
        assert_eq!(
            compute_relative_path("2026/01/05", "2025/12/31/23-59-59.thread.md"),
            "../../../2025/12/31/23-59-59.thread.md",
        );
    }

    #[test]
    fn test_empty_from_dir() {
        assert_eq!(
            compute_relative_path("", "2025/02/12/04-10-17.thread.md"),
            "2025/02/12/04-10-17.thread.md",
        );
    }
}
