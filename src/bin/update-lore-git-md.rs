use anyhow::Result;

use lore_git_md_helper::batch_import::process_emails;
use lore_git_md_helper::cat_file::CatFile;
use lore_git_md_helper::fast_import::FastImport;
use lore_git_md_helper::git_util::{resolve_ref, source_commit_from_ref};
use lore_git_md_helper::import_writer::write_fast_import;
use lore_git_md_helper::msgid_map::MsgIdMap;
use lore_git_md_helper::source_reader::read_source_emails;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: update-lore-git-md <source-repo> <target-repo> [range]");
        std::process::exit(1);
    }
    let source_repo = &args[1];
    let target_repo = &args[2];

    // Verify both repos exist
    let check = |path: &str| -> Result<()> {
        lore_git_md_helper::git_util::git(path, &["rev-parse", "--git-dir"])
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("not a git repo: {path}: {e:#}"))
    };
    check(source_repo)?;
    check(target_repo)?;

    let range = if let Some(r) = args.get(3) {
        r.clone()
    } else if let Some(last) = source_commit_from_ref(target_repo, "refs/heads/main") {
        eprintln!("resuming after {last}");
        format!("{last}..HEAD")
    } else {
        "HEAD".to_string()
    };

    eprintln!("source: {source_repo} range: {range}");
    eprintln!("target: {target_repo}");

    let emails = read_source_emails(source_repo, &range)?;
    eprintln!("{} emails to process", emails.len());

    if emails.is_empty() {
        eprintln!("nothing to do");
        return Ok(());
    }

    let mut existing_keys = lore_git_md_helper::datekey::load_existing_keys(target_repo)?;
    eprintln!("{} existing date-keys in target", existing_keys.len());

    let notes_cat = CatFile::new(target_repo)?;
    let mut map = MsgIdMap::new(Some(Box::new(notes_cat)));
    let mut target_cat = CatFile::new(target_repo)?;

    let result = process_emails(&emails, &mut map, &mut existing_keys, &mut target_cat);
    eprintln!(
        "{} emails converted, {} skipped, {} threads",
        result.emails.len(),
        result.skipped,
        result.trees.len(),
    );

    let mut fi = FastImport::new(target_repo, "refs/heads/main")?;
    if let Some(tip) = resolve_ref(target_repo, "refs/heads/main") {
        fi.set_parent(tip);
    }
    let mut notes_fi = fi.sibling("refs/notes/msgid");
    if let Some(tip) = resolve_ref(target_repo, "refs/notes/msgid") {
        notes_fi.set_parent(tip);
    }

    let notes_count = write_fast_import(&mut fi, &result, &map, &mut notes_fi, source_repo)?;
    map.clear_dirty();

    fi.finish()?;

    eprintln!(
        "Done! {} emails, {} threads, {} notes",
        result.emails.len(),
        result.trees.len(),
        notes_count,
    );

    Ok(())
}
