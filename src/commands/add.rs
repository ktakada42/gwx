//! `gwx add` — create a worktree, creating the branch when it does not exist.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cd_target;
use crate::cli::AddArgs;
use crate::config::normalize;
use crate::git;
use crate::hooks::{self, HookContext, Phase, Reporting};
use crate::repo::Repo;

/// How the branch of the new worktree is obtained.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Check out an existing local branch.
    Existing,
    /// Create a local branch tracking this remote-tracking branch.
    Track(String),
    /// Create a new branch from this start point.
    Create(String),
}

pub fn run(args: AddArgs) -> Result<()> {
    let repo = Repo::discover()?;
    let branch = args.branch.trim().to_string();
    if branch.is_empty() {
        bail!("branch name must not be empty");
    }
    reject_leading_dash("branch name", &branch)?;
    if let Some(start) = args.from.as_deref() {
        reject_leading_dash("start point", start)?;
    }
    if let Some(alias) = &args.alias {
        git::validate_alias(alias)?;
        let existing = repo.aliases()?;
        if let Some(other_branch) = existing.get(alias) {
            if other_branch != &branch && !args.force {
                bail!(
                    "alias `{alias}` is already assigned to `{other_branch}`. Use --force to overwrite."
                );
            }
        }
    }

    let path = match &args.path {
        Some(p) => normalize(&repo.cwd.join(p)),
        None => repo.worktree_path_for(&branch)?,
    };

    if let Some(existing) = repo
        .worktrees()?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    {
        if !args.force {
            bail!(
                "branch `{branch}` is already checked out at {}",
                existing.path.display()
            );
        }
    }
    if path.symlink_metadata().is_ok() {
        bail!("path already exists: {}", path.display());
    }

    let plan = decide_plan(&repo, &branch, args.from.as_deref(), args.no_create)?;

    let ctx = HookContext {
        main_worktree: repo.main.clone(),
        worktree_path: path.clone(),
        name: branch.clone(),
        branch: branch.clone(),
    };

    let reporting = if args.quiet {
        Reporting::Quiet
    } else {
        Reporting::Announce
    };

    if !args.no_hooks {
        hooks::run_all(
            &repo.config.hooks.pre_create,
            Phase::PreCreate,
            &ctx,
            reporting,
        )?;
    }

    // The directories leading to the worktree are git's to create. It makes
    // them itself, and only once it has accepted the branch name and the start
    // point — creating them here instead meant a name git was always going to
    // refuse still left an empty `worktrees/whatever` behind.
    git::run(
        &repo.main,
        git_args(&plan, &branch, &path, args.force, args.quiet),
    )
    .with_context(|| format!("failed to create worktree at {}", path.display()))?;

    if !args.quiet {
        match &plan {
            Plan::Existing => eprintln!("Checked out existing branch `{branch}`"),
            Plan::Track(remote) => eprintln!("Created branch `{branch}` tracking `{remote}`"),
            Plan::Create(start) => eprintln!("Created branch `{branch}` from `{start}`"),
        }
    }

    if let Some(alias) = &args.alias {
        git::set_alias(&repo.main, alias, &branch).with_context(|| left_behind(&path))?;
        if !args.quiet {
            eprintln!("Assigned alias `{alias}` to `{branch}`");
        }
    }

    if !args.no_hooks {
        hooks::run_all(
            &repo.config.hooks.post_create,
            Phase::PostCreate,
            &ctx,
            reporting,
        )
        // The hook's own error already names the phase; what it cannot say
        // is that the worktree survives it.
        .with_context(|| left_behind(&path))?;
    }

    // After the hooks rather than before them: the point of being moved is to
    // land in a worktree that is ready, not in one that is still installing.
    if args.cd {
        cd_target::request_beside_output(&path).with_context(|| left_behind(&path))?;
    }

    if args.quiet {
        println!("{}", path.display());
    } else {
        eprintln!("Worktree ready.");
        println!("{}", path.display());
    }
    Ok(())
}

/// What a failure past `git worktree add` has to say for itself: the worktree
/// is still there. That is what decides whether you go and look at it or run
/// the command again.
fn left_behind(path: &Path) -> String {
    format!("the worktree was created and is left at {}", path.display())
}

/// Refuses a name git would read as an option.
///
/// gwx puts `--` in front of the paths and refs it passes, but `git worktree
/// add` turns around and runs `git branch` itself, and that call is git's own.
/// A `-` there is an option, so `gwx add -- --detach` came back as a page of
/// usage text about a command the user never typed. No ref may begin with a
/// dash anyway, so nothing legitimate is turned away.
fn reject_leading_dash(what: &str, value: &str) -> Result<()> {
    if value.starts_with('-') {
        bail!("{what} must not start with `-`: `{value}`");
    }
    Ok(())
}

fn decide_plan(repo: &Repo, branch: &str, from: Option<&str>, no_create: bool) -> Result<Plan> {
    let local_exists = git::local_branch_exists(&repo.main, branch);

    if let Some(start) = from {
        if local_exists {
            bail!("branch `{branch}` already exists; omit --from to check it out");
        }
        if no_create {
            bail!("--no-create conflicts with --from");
        }
        return Ok(Plan::Create(start.to_string()));
    }

    if local_exists {
        return Ok(Plan::Existing);
    }

    let remotes = git::remote_branches_matching(&repo.main, branch)?;
    match remotes.len() {
        1 => Ok(Plan::Track(remotes.into_iter().next().unwrap())),
        0 => {
            if no_create {
                bail!("branch `{branch}` does not exist (--no-create was given)");
            }
            Ok(Plan::Create("HEAD".to_string()))
        }
        _ => bail!(
            "`{branch}` matches several remote branches: {}. Use --from to pick a start point",
            remotes.join(", ")
        ),
    }
}

fn git_args(plan: &Plan, branch: &str, path: &Path, force: bool, quiet: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if force {
        args.push("--force".into());
    }
    if quiet {
        args.push("--quiet".into());
    }
    // The ref after the path is the branch to check out, or the point the new
    // branch is cut from.
    let start = match plan {
        Plan::Existing => branch.to_string(),
        Plan::Track(remote) => {
            args.push("--track".into());
            args.push("-b".into());
            args.push(branch.to_string());
            remote.clone()
        }
        Plan::Create(start) => {
            args.push("-b".into());
            args.push(branch.to_string());
            start.clone()
        }
    };
    // Past `--` everything is a path or a ref. Nothing here comes from a shell,
    // so this is not about quoting: it is about a name that starts with a dash
    // being a name git rejects rather than an option git obeys.
    args.push("--".into());
    args.push(path.display().to_string());
    args.push(start);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_branch_is_checked_out() {
        let args = git_args(&Plan::Existing, "feat", Path::new("/wt/feat"), false, false);
        assert_eq!(args, ["worktree", "add", "--", "/wt/feat", "feat"]);
    }

    #[test]
    fn remote_branch_is_tracked() {
        let args = git_args(
            &Plan::Track("origin/feat".into()),
            "feat",
            Path::new("/wt/feat"),
            false,
            false,
        );
        assert_eq!(
            args,
            [
                "worktree",
                "add",
                "--track",
                "-b",
                "feat",
                "--",
                "/wt/feat",
                "origin/feat"
            ]
        );
    }

    #[test]
    fn new_branch_is_created_from_the_start_point() {
        let args = git_args(
            &Plan::Create("main".into()),
            "feat",
            Path::new("/wt/feat"),
            true,
            true,
        );
        assert_eq!(
            args,
            ["worktree", "add", "--force", "--quiet", "-b", "feat", "--", "/wt/feat", "main"]
        );
    }
}
