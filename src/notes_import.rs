use crate::fast_import::FastImport;
use crate::msgid_map::{MsgIdMap, format_note_value, hash_message_id};

/// Write a notes commit updating `refs/notes/msgid` with all dirty
/// entries from the map.
///
/// Uses the provided `FastImport` handle (which should target
/// `refs/notes/msgid`) to emit a single commit containing the
/// 3-level fanout tree entries.
///
/// Returns the number of notes entries written.
pub fn emit_notes_update(
    fi: &mut FastImport,
    map: &MsgIdMap,
    source_commit: Option<&str>,
) -> anyhow::Result<usize> {
    let dirty: Vec<(String, Option<String>)> = map
        .dirty_entries()
        .map(|(msgid, entry)| (msgid.to_string(), format_note_value(entry)))
        .collect();

    if dirty.is_empty() {
        return Ok(0);
    }

    let mut files: Vec<(String, String)> = Vec::new();
    for (msgid, value) in &dirty {
        let value = match value {
            Some(v) => v,
            None => continue, // Tombstones are not stored
        };

        let oid = hash_message_id(msgid);
        let (d1, rest) = oid.split_at(2);
        let (d2, d3) = rest.split_at(2);
        let path = format!("{d1}/{d2}/{d3}");
        files.push((path, value.clone()));
    }

    let msg = match source_commit {
        Some(oid) => format!("update msgid notes\n\nSource-Commit: {oid}"),
        None => "update msgid notes".to_string(),
    };

    let entries: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    fi.commit(&msg, &entries)?;

    Ok(files.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msgid_map::MsgIdEntry;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn test_fi() -> (FastImport, Rc<RefCell<Vec<u8>>>) {
        let buf = Rc::new(RefCell::new(Vec::new()));
        let fi = FastImport::from_writer(TestBuf(buf.clone()), "refs/notes/msgid");
        (fi, buf)
    }

    #[derive(Clone)]
    struct TestBuf(Rc<RefCell<Vec<u8>>>);

    impl std::io::Write for TestBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(data)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    #[test]
    fn test_emit_notes_empty() {
        let map = MsgIdMap::new(None);
        let (mut fi, buf) = test_fi();
        let count = emit_notes_update(&mut fi, &map, None).unwrap();
        assert_eq!(count, 0);
        assert!(buf.borrow().is_empty());
    }

    #[test]
    fn test_emit_notes_known_entry() {
        let mut map = MsgIdMap::new(None);
        map.insert(
            "test@example.com",
            MsgIdEntry::Known("2025/02/12/04-10-17".into()),
        );

        let (mut fi, buf) = test_fi();
        let count = emit_notes_update(&mut fi, &map, Some("abc123")).unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(output.contains("commit refs/notes/msgid\n"));
        assert!(output.contains("M 100644 inline "));
        assert!(output.contains("2025/02/12/04-10-17"));
        assert!(output.contains("Source-Commit: abc123"));
    }

    #[test]
    fn test_emit_notes_wanted_entry() {
        let mut map = MsgIdMap::new(None);
        map.insert(
            "root@example.com",
            MsgIdEntry::WantedBy(vec!["2025/02/10/00-00-00".into()]),
        );

        let (mut fi, buf) = test_fi();
        let count = emit_notes_update(&mut fi, &map, None).unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(output.contains("wanted-by\n2025/02/10/00-00-00\n"));
    }

    #[test]
    fn test_emit_notes_skips_tombstone() {
        let mut map = MsgIdMap::new(None);
        map.insert("gone@example.com", MsgIdEntry::Tombstone);

        let (mut fi, _buf) = test_fi();
        let count = emit_notes_update(&mut fi, &map, None).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_emit_notes_no_parent() {
        let mut map = MsgIdMap::new(None);
        map.insert("x@y.com", MsgIdEntry::Known("2025/01/01/00-00-00".into()));

        let (mut fi, buf) = test_fi();
        emit_notes_update(&mut fi, &map, None).unwrap();

        let output = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(
            !output.contains("from "),
            "fresh handle should not emit a from line"
        );
    }
}
