use crate::msgid_map::{MsgIdEntry, MsgIdMap};

/// Extract the list of Message-IDs from the References header.
///
/// Returns them in header order: root-most first, direct-parent last.
pub fn get_references(message: &mail_parser::Message) -> Vec<String> {
    message
        .references()
        .as_text_list()
        .unwrap_or_default()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Resolve the thread root for an email given its references chain.
///
/// Walks the chain from root-most (first) to leaf-most (last) and finds the
/// closest-to-root entry that is `Known` in the map.  Unknown references are
/// marked as `WantedBy` so we can re-root threads when those emails arrive.
///
/// Returns the date-key of the thread root, or `None` if this email appears
/// to be a thread root itself (no known ancestors).
pub fn resolve_thread_root(
    refs: &[String],
    my_date_key: &str,
    map: &mut MsgIdMap,
) -> Option<String> {
    let mut root_dk: Option<String> = None;

    for msgid in refs {
        let entry = map.get(msgid).clone();
        match entry {
            MsgIdEntry::Known(dk) => {
                root_dk.get_or_insert(dk);
            }
            MsgIdEntry::WantedBy(mut dks) => {
                if !dks.contains(&my_date_key.to_string()) {
                    dks.push(my_date_key.to_string());
                    map.insert(msgid, MsgIdEntry::WantedBy(dks));
                }
            }
            MsgIdEntry::Tombstone => {
                map.insert(msgid, MsgIdEntry::WantedBy(vec![my_date_key.to_string()]));
            }
        }
    }

    root_dk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map() -> MsgIdMap {
        MsgIdMap::new(None)
    }

    #[test]
    fn test_resolve_no_refs() {
        let mut map = make_map();
        let root = resolve_thread_root(&[], "2025/02/12/04-10-17", &mut map);
        assert_eq!(root, None);
    }

    #[test]
    fn test_resolve_known_root() {
        let mut map = make_map();
        map.insert(
            "root@example.com",
            MsgIdEntry::Known("2025/01/01/00-00-00".into()),
        );
        map.clear_dirty();

        let refs = vec!["root@example.com".to_string()];
        let root = resolve_thread_root(&refs, "2025/02/12/04-10-17", &mut map);
        assert_eq!(root, Some("2025/01/01/00-00-00".to_string()));

        // Known entry was not modified, so no dirty entries
        assert_eq!(map.dirty_entries().count(), 0);
    }

    #[test]
    fn test_resolve_unknown_ref_becomes_wanted() {
        let mut map = make_map();

        let refs = vec!["unknown@example.com".to_string()];
        let root = resolve_thread_root(&refs, "2025/02/12/04-10-17", &mut map);
        assert_eq!(root, None);

        assert_eq!(
            map.get("unknown@example.com"),
            &MsgIdEntry::WantedBy(vec!["2025/02/12/04-10-17".to_string()]),
        );
        // Should be dirty
        assert!(
            map.dirty_entries()
                .any(|(id, _)| id == "unknown@example.com")
        );
    }

    #[test]
    fn test_resolve_mixed_chain() {
        let mut map = make_map();
        map.insert(
            "root@example.com",
            MsgIdEntry::Known("2025/01/01/00-00-00".into()),
        );
        map.clear_dirty();

        let refs = vec![
            "root@example.com".to_string(),
            "middle@example.com".to_string(),
            "parent@example.com".to_string(),
        ];
        let root = resolve_thread_root(&refs, "2025/02/12/04-10-17", &mut map);
        assert_eq!(root, Some("2025/01/01/00-00-00".to_string()));

        // middle and parent should be WantedBy and dirty
        assert_eq!(
            map.get("middle@example.com"),
            &MsgIdEntry::WantedBy(vec!["2025/02/12/04-10-17".to_string()]),
        );
        assert_eq!(
            map.get("parent@example.com"),
            &MsgIdEntry::WantedBy(vec!["2025/02/12/04-10-17".to_string()]),
        );
        assert_eq!(map.dirty_entries().count(), 2);
    }

    #[test]
    fn test_resolve_picks_closest_to_root() {
        let mut map = make_map();
        map.insert(
            "root@example.com",
            MsgIdEntry::Known("2025/01/01/00-00-00".into()),
        );
        map.insert(
            "parent@example.com",
            MsgIdEntry::Known("2025/02/01/00-00-00".into()),
        );
        map.clear_dirty();

        let refs = vec![
            "root@example.com".to_string(),
            "parent@example.com".to_string(),
        ];
        let root = resolve_thread_root(&refs, "2025/02/12/04-10-17", &mut map);
        assert_eq!(root, Some("2025/01/01/00-00-00".to_string()));
    }

    #[test]
    fn test_resolve_accumulates_wanted_by() {
        let mut map = make_map();

        let refs = vec!["root@example.com".to_string()];
        resolve_thread_root(&refs, "2025/02/10/00-00-00", &mut map);
        resolve_thread_root(&refs, "2025/02/11/00-00-00", &mut map);

        assert_eq!(
            map.get("root@example.com"),
            &MsgIdEntry::WantedBy(vec![
                "2025/02/10/00-00-00".to_string(),
                "2025/02/11/00-00-00".to_string(),
            ]),
        );
    }

    #[test]
    fn test_resolve_does_not_duplicate_wanted_by() {
        let mut map = make_map();

        let refs = vec!["root@example.com".to_string()];
        resolve_thread_root(&refs, "2025/02/10/00-00-00", &mut map);
        resolve_thread_root(&refs, "2025/02/10/00-00-00", &mut map);

        assert_eq!(
            map.get("root@example.com"),
            &MsgIdEntry::WantedBy(vec!["2025/02/10/00-00-00".to_string()]),
        );
    }
}
