//! Candidates for dynamic shell completion.
//!
//! These run in a separate process that the shell spawns on every `<TAB>`, so
//! they must be quick and must never fail: outside a repository, or when git
//! misbehaves, the answer is simply "no candidates".

use std::collections::BTreeMap;

use clap_complete::CompletionCandidate;

use crate::git;
use crate::repo::{Repo, MAIN_ALIAS};

/// Worktrees `gwx cd` accepts, including `@` for the main worktree.
pub fn worktrees() -> Vec<CompletionCandidate> {
    candidates(false)
}

/// Worktrees `gwx remove` accepts — everything but the main worktree.
pub fn removable_worktrees() -> Vec<CompletionCandidate> {
    candidates(true)
}

fn candidates(skip_main: bool) -> Vec<CompletionCandidate> {
    let Ok(repo) = Repo::discover() else {
        return Vec::new();
    };
    let Ok(worktrees) = repo.worktrees() else {
        return Vec::new();
    };
    let Some(main) = worktrees.first() else {
        return Vec::new();
    };
    let aliases = repo.aliases().unwrap_or_default();
    let branch_to_alias: BTreeMap<&str, &str> = aliases
        .iter()
        .map(|(alias, branch)| (branch.as_str(), alias.as_str()))
        .collect();

    let mut result = Vec::new();
    for wt in worktrees.iter().skip(usize::from(skip_main)) {
        if wt.path == main.path {
            result.push(CompletionCandidate::new(MAIN_ALIAS).help(Some(
                format!("main worktree — {}", wt.path.display()).into(),
            )));
            continue;
        }

        let branch = wt.branch.as_deref().unwrap_or("");
        let alias = if !branch.is_empty() {
            branch_to_alias.get(branch).copied()
        } else {
            None
        };

        if let Some(alias) = alias {
            let alias_hint = if !branch.is_empty() {
                format!("alias for {branch} — {}", wt.path.display())
            } else {
                wt.path.display().to_string()
            };
            result.push(CompletionCandidate::new(alias).help(Some(alias_hint.into())));
            if !branch.is_empty() {
                result.push(
                    CompletionCandidate::new(branch)
                        .help(Some(wt.path.display().to_string().into())),
                );
            }
        } else {
            let name = repo.display_name(wt, main);
            result.push(
                CompletionCandidate::new(name).help(Some(wt.path.display().to_string().into())),
            );
        }
    }
    result
}

/// Branches `gwx add` can turn into a worktree.
///
/// Branches that already have a worktree are left out — `gwx add` would refuse
/// them anyway. Remote-only branches are offered under their short name, which
/// is exactly what `gwx add` expects.
pub fn addable_branches() -> Vec<CompletionCandidate> {
    let Ok(repo) = Repo::discover() else {
        return Vec::new();
    };
    let Ok(worktrees) = repo.worktrees() else {
        return Vec::new();
    };
    let in_use: Vec<String> = worktrees.iter().filter_map(|w| w.branch.clone()).collect();

    let locals = git::local_branches(&repo.cwd).unwrap_or_default();
    let mut candidates: Vec<CompletionCandidate> = locals
        .iter()
        .filter(|b| !in_use.contains(b))
        .map(|b| CompletionCandidate::new(b).help(Some("local branch".into())))
        .collect();

    // A branch name can live on several remotes; `gwx add` takes the short name,
    // so those collapse into one candidate rather than appearing once per remote.
    let mut by_short: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (full, short) in git::remote_branches(&repo.cwd).unwrap_or_default() {
        if locals.contains(&short) || in_use.contains(&short) {
            continue;
        }
        by_short.entry(short).or_default().push(full);
    }

    for (short, remotes) in by_short {
        let help = if remotes.len() == 1 {
            remotes[0].clone()
        } else {
            // `gwx add` cannot pick between them; say so instead of looking usable.
            format!("{} — ambiguous, needs --from", remotes.join(", "))
        };
        candidates.push(CompletionCandidate::new(short).help(Some(help.into())));
    }
    candidates
}

/// Refs `gwx add --from` accepts: branches, tags and remote-tracking branches.
pub fn start_points() -> Vec<CompletionCandidate> {
    let Ok(repo) = Repo::discover() else {
        return Vec::new();
    };
    git::start_points(&repo.cwd)
        .unwrap_or_default()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}
