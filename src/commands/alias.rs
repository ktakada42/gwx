//! `gwx alias` — manage worktree aliases.

use anyhow::{anyhow, bail, Result};

use crate::cli::AliasArgs;
use crate::git;
use crate::repo::Repo;

pub fn run(args: AliasArgs) -> Result<()> {
    let repo = Repo::discover()?;

    if args.unset {
        return unset_alias(&repo, args.target.as_deref(), args.alias.as_deref());
    }

    match (args.target, args.alias) {
        (None, None) => list_aliases(&repo),
        (Some(alias), None) => set_alias(&repo, None, &alias, args.force),
        (Some(target), Some(alias)) => set_alias(&repo, Some(&target), &alias, args.force),
        (None, Some(_)) => unreachable!(),
    }
}

fn list_aliases(repo: &Repo) -> Result<()> {
    let aliases = repo.aliases()?;
    if aliases.is_empty() {
        return Ok(());
    }

    let max_len = aliases.keys().map(|k| k.len()).max().unwrap_or(0);
    for (alias, branch) in aliases {
        println!("{alias:<max_len$} -> {branch}");
    }
    Ok(())
}

fn set_alias(repo: &Repo, target: Option<&str>, alias: &str, force: bool) -> Result<()> {
    git::validate_alias(alias)?;

    let target_wt = match target {
        Some(t) => repo.resolve(Some(t))?,
        None => {
            let worktrees = repo.worktrees()?;
            worktrees
                .into_iter()
                .find(|w| w.path == repo.cwd || repo.cwd.starts_with(&w.path))
                .ok_or_else(|| anyhow!("not standing inside a worktree"))?
        }
    };

    let branch = target_wt
        .branch
        .ok_or_else(|| anyhow!("cannot alias a detached or bare worktree"))?;

    let existing = repo.aliases()?;
    if let Some(other_branch) = existing.get(alias) {
        if other_branch != &branch && !force {
            bail!(
                "alias `{alias}` is already assigned to `{other_branch}`. Use --force to overwrite."
            );
        }
    }

    // If this branch already had another alias, remove the old one first.
    for (old_alias, target_branch) in &existing {
        if target_branch == &branch && old_alias != alias {
            git::remove_alias(&repo.main, old_alias)?;
        }
    }

    git::set_alias(&repo.main, alias, &branch)?;
    eprintln!("Assigned alias `{alias}` to `{branch}`");
    Ok(())
}

fn unset_alias(repo: &Repo, target: Option<&str>, extra_alias: Option<&str>) -> Result<()> {
    if extra_alias.is_some() {
        bail!("`--unset` accepts at most one worktree argument");
    }

    let target_wt = match target {
        Some(t) => repo.resolve(Some(t))?,
        None => {
            let worktrees = repo.worktrees()?;
            worktrees
                .into_iter()
                .find(|w| w.path == repo.cwd || repo.cwd.starts_with(&w.path))
                .ok_or_else(|| anyhow!("not standing inside a worktree"))?
        }
    };

    let branch = target_wt
        .branch
        .ok_or_else(|| anyhow!("cannot alias a detached or bare worktree"))?;

    let existing = repo.aliases()?;
    let mut found = false;
    for (alias, target_branch) in existing {
        if target_branch == branch {
            git::remove_alias(&repo.main, &alias)?;
            eprintln!("Removed alias `{alias}` for `{branch}`");
            found = true;
        }
    }

    if !found {
        eprintln!("No alias found for `{branch}`");
    }
    Ok(())
}
