use std::collections::HashMap;

use crate::cat_file::BlobRead;

/// The state of a Message-ID in the in-memory map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsgIdEntry {
    /// Resolved to a date-key (e.g. "2025/02/12/04-10-17").
    Known(String),
    /// Not yet seen; these date-keys' threads reference it as root.
    WantedBy(Vec<String>),
    /// Looked up in notes and not found.
    Tombstone,
}

/// Lazy map from Message-ID to date-key, backed by `refs/notes/msgid`.
///
/// Reads from the in-memory cache first, falling back to a persistent
/// `git cat-file --batch` process on cache miss.
pub struct MsgIdMap {
    cache: HashMap<String, MsgIdEntry>,
    cat: Option<Box<dyn BlobRead>>,
    /// Number of note lookups via cat-file (for profiling).
    pub note_lookups: u64,
}

impl MsgIdMap {
    pub fn new(cat: Option<Box<dyn BlobRead>>) -> Self {
        Self {
            cache: HashMap::new(),
            cat,
            note_lookups: 0,
        }
    }

    /// Insert or update an entry.
    pub fn insert(&mut self, message_id: &str, entry: MsgIdEntry) {
        self.cache.insert(message_id.to_string(), entry);
    }

    /// Register a Message-ID as Known, returning its previous state.
    ///
    /// This is the primary way to record a newly-seen email.  If the old
    /// state was `WantedBy(date_keys)`, callers must handle re-rooting
    /// those threads.
    pub fn insert_known(&mut self, message_id: &str, date_key: String) -> Option<MsgIdEntry> {
        // Ensure the entry is in cache (may trigger notes lookup)
        if !self.cache.contains_key(message_id) {
            let entry = self.lookup_note(message_id);
            self.cache.insert(message_id.to_string(), entry);
        }
        self.cache
            .insert(message_id.to_string(), MsgIdEntry::Known(date_key))
    }

    /// Look up a Message-ID, falling back to git notes on cache miss.
    pub fn get(&mut self, message_id: &str) -> &MsgIdEntry {
        if !self.cache.contains_key(message_id) {
            let entry = self.lookup_note(message_id);
            self.cache.insert(message_id.to_string(), entry);
        }
        self.cache.get(message_id).unwrap()
    }

    /// Look up a Message-ID in `refs/notes/msgid` via the persistent
    /// cat-file process, using our 3-level fanout (ab/cd/ef...).
    fn lookup_note(&mut self, message_id: &str) -> MsgIdEntry {
        self.note_lookups += 1;
        let cat = match self.cat.as_mut() {
            Some(c) => c,
            None => return MsgIdEntry::Tombstone,
        };
        let oid = hash_message_id(message_id);
        let (d1, rest) = oid.split_at(2);
        let (d2, d3) = rest.split_at(2);
        let spec = format!("refs/notes/msgid:{d1}/{d2}/{d3}");
        match cat.get_str(&spec) {
            Some(text) => parse_note_value(text.trim()),
            None => MsgIdEntry::Tombstone,
        }
    }
}

/// Parse the text stored in a note into a `MsgIdEntry`.
pub fn parse_note_value(text: &str) -> MsgIdEntry {
    if text.starts_with("wanted-by") {
        let date_keys: Vec<String> = text
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty() && looks_like_date_key(l))
            .map(|l| l.to_string())
            .collect();
        if date_keys.is_empty() {
            return MsgIdEntry::Tombstone;
        }
        MsgIdEntry::WantedBy(date_keys)
    } else if looks_like_date_key(text) {
        MsgIdEntry::Known(text.to_string())
    } else {
        MsgIdEntry::Tombstone
    }
}

/// Check whether a string looks like a date-key (YYYY/MM/DD/HH-MM-SS…).
fn looks_like_date_key(s: &str) -> bool {
    let bytes = s.as_bytes();
    // Minimum: "YYYY/MM/DD/HH-MM-SS" = 19 chars
    if bytes.len() < 19 {
        return false;
    }
    // Check separators
    if bytes[4] != b'/'
        || bytes[7] != b'/'
        || bytes[10] != b'/'
        || bytes[13] != b'-'
        || bytes[16] != b'-'
    {
        return false;
    }
    // Check that the fixed positions are digits
    [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
        .iter()
        .all(|&i| bytes[i].is_ascii_digit())
}

/// Compute the SHA-1 of a Message-ID as Git would for `git hash-object --stdin`.
///
/// Git blob hash = SHA-1(`blob <len>\0<content>`) where content = message_id + "\n".
pub fn hash_message_id(message_id: &str) -> String {
    let payload = format!("{message_id}\n");
    let header = format!("blob {}\0", payload.len());
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(payload.as_bytes());
    let digest = hasher.digest().bytes();
    let mut hex = String::with_capacity(40);
    for b in &digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Format a `MsgIdEntry` as the text to store in a git note.
pub fn format_note_value(entry: &MsgIdEntry) -> Option<String> {
    match entry {
        MsgIdEntry::Known(dk) => Some(dk.clone()),
        MsgIdEntry::WantedBy(dks) => {
            let mut s = String::from("wanted-by\n");
            for dk in dks {
                s.push_str(dk);
                s.push('\n');
            }
            Some(s)
        }
        MsgIdEntry::Tombstone => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_message_id() {
        // Verify against: echo -n "<msg-id>" | git hash-object --stdin
        let h1 = hash_message_id("20250212041017.91370-1-test@example.com");
        assert_eq!(h1, "1e444ba644f1e99cb5212d76de08c418967bd06a");
        let h2 = hash_message_id("20250212041017.91370-1-test@example.com");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_parse_note_known() {
        assert_eq!(
            parse_note_value("2025/02/12/04-10-17"),
            MsgIdEntry::Known("2025/02/12/04-10-17".to_string()),
        );
    }

    #[test]
    fn test_parse_note_wanted_by() {
        let text = "wanted-by\n2025/02/12/04-10-17\n2025/02/13/10-00-00\n";
        assert_eq!(
            parse_note_value(text),
            MsgIdEntry::WantedBy(vec![
                "2025/02/12/04-10-17".to_string(),
                "2025/02/13/10-00-00".to_string(),
            ]),
        );
    }

    #[test]
    fn test_parse_note_tombstone() {
        assert_eq!(parse_note_value("garbage"), MsgIdEntry::Tombstone);
    }

    #[test]
    fn test_format_note_value_known() {
        let entry = MsgIdEntry::Known("2025/02/12/04-10-17".to_string());
        assert_eq!(
            format_note_value(&entry),
            Some("2025/02/12/04-10-17".to_string())
        );
    }

    #[test]
    fn test_format_note_value_wanted() {
        let entry = MsgIdEntry::WantedBy(vec![
            "2025/02/12/04-10-17".to_string(),
            "2025/02/13/10-00-00".to_string(),
        ]);
        assert_eq!(
            format_note_value(&entry),
            Some("wanted-by\n2025/02/12/04-10-17\n2025/02/13/10-00-00\n".to_string()),
        );
    }

    #[test]
    fn test_format_note_value_tombstone() {
        assert_eq!(format_note_value(&MsgIdEntry::Tombstone), None);
    }

    #[test]
    fn test_looks_like_date_key() {
        assert!(looks_like_date_key("2025/02/12/04-10-17"));
        assert!(looks_like_date_key("2025/02/12/04-10-17-1"));
        assert!(!looks_like_date_key("wanted-by"));
        assert!(!looks_like_date_key("short"));
    }

    #[test]
    fn test_msgid_map_insert_and_get() {
        let mut map = MsgIdMap::new(None);
        map.insert(
            "test@example.com",
            MsgIdEntry::Known("2025/01/01/00-00-00".to_string()),
        );
        assert_eq!(
            map.get("test@example.com"),
            &MsgIdEntry::Known("2025/01/01/00-00-00".to_string()),
        );
    }

    #[test]
    fn test_msgid_map_miss_becomes_tombstone() {
        let mut map = MsgIdMap::new(None);
        // Notes lookup will fail -> Tombstone
        assert_eq!(map.get("unknown@example.com"), &MsgIdEntry::Tombstone);
    }

    #[test]
    fn test_insert_known_returns_old_wanted_by() {
        let mut map = MsgIdMap::new(None);
        map.insert(
            "root@example.com",
            MsgIdEntry::WantedBy(vec!["2025/02/10/00-00-00".into()]),
        );

        let old = map.insert_known("root@example.com", "2025/02/12/04-10-17".into());
        assert_eq!(
            old,
            Some(MsgIdEntry::WantedBy(vec![
                "2025/02/10/00-00-00".to_string()
            ])),
        );
        assert_eq!(
            map.get("root@example.com"),
            &MsgIdEntry::Known("2025/02/12/04-10-17".to_string()),
        );
    }

    #[test]
    fn test_insert_known_returns_none_for_new() {
        let mut map = MsgIdMap::new(None);
        // First time seeing this message-id (notes lookup will fail -> Tombstone)
        let old = map.insert_known("new@example.com", "2025/01/01/00-00-00".into());
        assert_eq!(old, Some(MsgIdEntry::Tombstone));
    }
}
