//! `gwx remove` — delete a worktree, optionally with its branch.

use anyhow::{bail, Context, Result};

use crate::cli::RemoveArgs;
use crate::git::{self, Merged, Worktree};
use crate::hooks::{self, HookContext, Phase, Reporting};
use crate::repo::Repo;

/// What a removal is allowed to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOptions {
    /// Remove despite uncommitted changes, and delete an unmerged branch.
    pub force: bool,
    /// Delete the branch along with the worktree.
    pub with_branch: bool,
    /// Keep git quiet — the interactive picker owns the terminal.
    pub quiet: bool,
    /// Skip the pre_remove and post_remove hooks.
    pub no_hooks: bool,
}

/// Refuses removals that would leave the user stranded or lose work.
///
/// Split out so the picker can show the same reasons before asking for
/// confirmation, rather than discovering them after the fact.
pub fn removal_blocker(repo: &Repo, worktree: &Worktree, force: bool) -> Option<String> {
    if worktree.path == repo.main {
        return Some("this is the main worktree".to_string());
    }
    if repo.cwd.starts_with(&worktree.path) {
        return Some("you are inside this worktree".to_string());
    }
    // A broken checkout cannot be asked what it is holding: `git status`
    // fails on it, so the dirty check below would wave through work that is
    // about to be deleted along with the directory.
    if !force && worktree.prunable {
        return Some(
            "its checkout is broken and gwx cannot tell whether it holds uncommitted work; pass --force to delete it anyway"
                .to_string(),
        );
    }
    if !force && git::is_dirty(&worktree.path).unwrap_or(false) {
        return Some("it has uncommitted changes".to_string());
    }
    None
}

/// Removes a worktree, and its branch when asked.
///
/// The hooks bracket the whole removal: `pre_remove` while the worktree is
/// still there, so it can stop what is running inside it or make the files
/// deletable, and `post_remove` once the worktree — and the branch, when
/// `--with-branch` was given — is gone, so cleanup outside the repository has
/// nothing left to trip over.
pub fn remove_worktree(repo: &Repo, worktree: &Worktree, opts: RemoveOptions) -> Result<()> {
    if let Some(reason) = removal_blocker(repo, worktree, opts.force) {
        bail!("cannot remove {}: {reason}", worktree.path.display());
    }

    let ctx = HookContext {
        main_worktree: repo.main.clone(),
        worktree_path: worktree.path.clone(),
        name: worktree.name(),
        branch: worktree.branch.clone().unwrap_or_default(),
    };
    let reporting = if opts.quiet {
        Reporting::Captured
    } else {
        Reporting::Announce
    };

    if !opts.no_hooks {
        hooks::run_all(
            &repo.config.hooks.pre_remove,
            Phase::PreRemove,
            &ctx,
            reporting,
        )?;
    }

    // git will not `remove` a worktree whose `.git` has gone missing, however
    // many times `--force` is passed: validation runs before the flag is read
    // and fails on the absent file. Deleting the directory ourselves leaves
    // the one kind of breakage git does handle — no directory at all — and
    // the command below then drops the administrative record as usual.
    // `git worktree prune` would clear the record too, but it works on every
    // prunable worktree in the repository, and only this one was asked for.
    if worktree.prunable {
        match std::fs::remove_dir_all(&worktree.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to delete {}", worktree.path.display()))
            }
        }
    }

    let mut git_args = vec!["worktree".to_string(), "remove".to_string()];
    if opts.force {
        git_args.push("--force".to_string());
    }
    // Past `--` the argument is a path, whatever it starts with.
    git_args.push("--".to_string());
    git_args.push(worktree.path.display().to_string());
    run_git(&repo.main, git_args, opts.quiet)
        .with_context(|| format!("failed to remove {}", worktree.path.display()))?;

    if opts.with_branch {
        // Everything past here happens with the worktree already gone, so a
        // failure has to say so: otherwise `--with-branch` looks like it did
        // nothing at all, and the next attempt starts from a state the
        // message never mentioned.
        let removed = || format!("the worktree at {} was removed", worktree.path.display());
        let Some(branch) = worktree.branch.as_deref() else {
            bail!("{}, but it had no branch to remove", removed());
        };
        let merged = git::MergeState::read(&repo.main)?.of(branch);
        if !opts.force && merged == Merged::No {
            bail!(
                "{}, but branch `{branch}` is not merged into HEAD of the main worktree and was kept; pass --force to delete it too",
                removed()
            );
        }
        // `git branch -d` runs the same ancestry test that misses a squash
        // merge, so it would refuse the branch gwx has just decided is safe.
        // Reaching for `-D` means gwx answers for the deletion, which is why
        // the check behind `Merged::Changes` is the one that never says yes to
        // work that was merged and then reverted.
        let flag = if merged == Merged::Commits && !opts.force {
            "-d"
        } else {
            "-D"
        };
        run_git(&repo.main, ["branch", flag, "--", branch], opts.quiet)
            .with_context(|| format!("failed to delete branch `{branch}`"))
            .with_context(removed)?;
    }

    if !opts.no_hooks {
        hooks::run_all(
            &repo.config.hooks.post_remove,
            Phase::PostRemove,
            &ctx,
            reporting,
        )
        .with_context(|| {
            let branch = if opts.with_branch {
                " and its branch"
            } else {
                ""
            };
            format!(
                "the worktree at {}{branch} was already removed",
                worktree.path.display()
            )
        })?;
    }
    Ok(())
}

fn run_git<I, S>(dir: &std::path::Path, args: I, quiet: bool) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    if quiet {
        git::output(dir, args).map(|_| ())
    } else {
        git::run(dir, args)
    }
}

pub fn run(args: RemoveArgs) -> Result<()> {
    let repo = Repo::discover()?;
    let worktree = repo.resolve(Some(&args.name))?;

    // Keep the CLI's specific wording; the picker uses `removal_blocker`.
    if worktree.path == repo.main {
        bail!("refusing to remove the main worktree");
    }
    if repo.cwd.starts_with(&worktree.path) {
        bail!(
            "you are inside {}. Move out of it first (try `gwx cd @`)",
            worktree.path.display()
        );
    }
    if !args.force && git::is_dirty(&worktree.path).unwrap_or(false) {
        bail!(
            "{} has uncommitted changes. Commit them or pass --force",
            worktree.path.display()
        );
    }

    remove_worktree(
        &repo,
        &worktree,
        RemoveOptions {
            force: args.force,
            with_branch: args.with_branch,
            quiet: false,
            no_hooks: args.no_hooks,
        },
    )?;
    eprintln!("Removed {}", worktree.path.display());
    if args.with_branch {
        if let Some(branch) = worktree.branch.as_deref() {
            eprintln!("Deleted branch `{branch}`");
        }
    }
    Ok(())
}
