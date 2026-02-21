use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: update-lore-git-md <source-repo> <target-repo>");
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

    eprintln!("source: {source_repo}");
    eprintln!("target: {target_repo}");
    eprintln!("(not yet implemented)");

    Ok(())
}
