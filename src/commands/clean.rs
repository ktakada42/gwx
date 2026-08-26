//! `gwx clean` — review the worktrees that have outlived their branch and
//! remove the ones you pick.
//!
//! The judgement it makes rests on one fact: removing a worktree does not
//! remove commits. The branch keeps them, so the only thing a removal can
//! destroy is work that was never committed. Everything else the list reports
//! is about whether the work is *finished*, which is a different question and
//! the reason nothing but `done` is selected for you.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::cli::CleanArgs;
use crate::commands::remove::{removal_blocker, remove_worktree, RemoveOptions};
use crate::git::{self, Merged, Tracking, Worktree};
use crate::repo::Repo;
use crate::tui;

/// What a worktree's state means for removing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Merged into the main worktree's HEAD, with nothing uncommitted.
    Done,
    /// Not merged, but every commit is on the upstream.
    Pushed,
    /// Commits that exist nowhere else, or no upstream at all.
    Local,
    /// Uncommitted changes, which a removal would destroy.
    Dirty,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Done => "done",
            State::Pushed => "pushed",
            State::Local => "local",
            State::Dirty => "dirty",
        }
    }

    /// The column that answers the question the screen is asking.
    ///
    /// `dirty` is the only state where removing the worktree destroys
    /// anything, so it is the only "no" — until `--with-branch` is given, at
    /// which point a `local` branch takes its commits with it and becomes one
    /// too. The state stays in brackets rather than being replaced by the
    /// verdict, because "no" on its own does not say what to do about it.
    pub fn verdict(self, with_branch: bool) -> String {
        let safe = match self {
            State::Dirty => false,
            State::Local => !with_branch,
            State::Done | State::Pushed => true,
        };
        let yes_no = if safe { "yes" } else { "no" };
        match self {
            State::Done => yes_no.to_string(),
            other => format!("{yes_no} ({})", other.label()),
        }
    }

    /// Whether the row starts out ticked.
    ///
    /// Only `done`. The others are removable — `pushed` and `local` lose
    /// nothing, since the commits outlive the worktree — but they are live
    /// work, and a list that pre-selects live work gets used once.
    pub fn preselected(self) -> bool {
        self == State::Done
    }
}

/// A worktree offered for removal, with the reason for its state.
pub struct Candidate {
    pub worktree: Worktree,
    pub name: String,
    pub state: State,
    pub note: String,
}

pub fn run(args: CleanArgs) -> Result<()> {
    let repo = Repo::discover()?;
    let candidates = candidates(&repo)?;

    if candidates.is_empty() {
        eprintln!("Nothing to clean: no worktree besides the one you are in.");
        return Ok(());
    }

    // Without a terminal there is nobody to tick the boxes, and guessing is
    // the one thing a command that deletes things must not do.
    if !tui::is_available() {
        print_table(&candidates, args.with_branch);
        eprintln!();
        eprintln!("gwx clean needs a terminal to choose in; nothing was removed.");
        return Ok(());
    }

    let Some(chosen) = tui::choose_to_clean(&candidates, args.with_branch)? else {
        return Ok(());
    };
    if chosen.is_empty() {
        eprintln!("Nothing selected.");
        return Ok(());
    }

    let mut removed = 0;
    for index in chosen {
        let candidate = &candidates[index];
        if candidate.state == State::Dirty && !args.force {
            eprintln!(
                "Skipped {}: it has uncommitted changes (pass --force)",
                candidate.name
            );
            continue;
        }
        let opts = RemoveOptions {
            force: args.force,
            with_branch: args.with_branch,
            quiet: false,
            no_hooks: args.no_hooks,
        };
        match remove_worktree(&repo, &candidate.worktree, opts) {
            Ok(()) => {
                removed += 1;
                eprintln!("Removed {}", candidate.worktree.path.display());
            }
            // One failure should not strand the rest of the selection: the
            // user asked for several removals, not for a transaction.
            Err(e) => eprintln!("Failed to remove {}: {e:#}", candidate.name),
        }
    }
    if removed != 1 {
        eprintln!("Removed {removed} worktrees.");
    }
    Ok(())
}

/// Every worktree that could be removed, with its state worked out.
pub fn candidates(repo: &Repo) -> Result<Vec<Candidate>> {
    let worktrees = repo.worktrees()?;
    let Some(main) = worktrees.first().cloned() else {
        bail!("no worktrees found");
    };

    // Repository-wide calls, made once rather than once per worktree.
    let merges = git::MergeState::read(&repo.main)?;
    let tracking = git::tracking(&repo.main)?;

    // What is left is per worktree, and on a large working tree it is the
    // whole cost of the command: `git status` walks every tracked file, so a
    // monorepo with twelve worktrees spent seconds here one worktree at a
    // time. They do not depend on each other, so they run at once — the same
    // fan-out the picker already uses to fill its status column.
    //
    // The blocker check stays out here: with `force` it compares paths and
    // asks git nothing, and a worktree it turns down needs no thread.
    let removable: Vec<Worktree> = worktrees
        .into_iter()
        .skip(1)
        .filter(|worktree| removal_blocker(repo, worktree, true).is_none())
        .collect();

    Ok(std::thread::scope(|scope| {
        // Every worker reads the same three answers and owns none of them.
        let (main, merges, tracking) = (&main, &merges, &tracking);
        let workers: Vec<_> = removable
            .iter()
            .map(|worktree| scope.spawn(move || describe(repo, worktree, main, merges, tracking)))
            .collect();
        workers.into_iter().filter_map(|w| w.join().ok()).collect()
    }))
}

/// Works out one worktree's state, on a thread of its own.
///
/// The two questions it asks git — is the working tree dirty, and is the
/// branch's work on `HEAD` — are the ones that cost anything, and neither
/// needs an answer about any other worktree.
fn describe(
    repo: &Repo,
    worktree: &Worktree,
    main: &Worktree,
    merges: &git::MergeState,
    tracking: &BTreeMap<String, Tracking>,
) -> Candidate {
    let dirty = git::is_dirty(&worktree.path).unwrap_or(false);
    let branch = worktree.branch.as_deref();
    let merged = branch.map_or(Merged::No, |b| merges.of(b));
    let track = branch
        .and_then(|b| tracking.get(b).copied())
        .unwrap_or(Tracking::Untracked);

    let (state, note) = classify(dirty, merged, track);
    Candidate {
        name: repo.display_name(worktree, main),
        worktree: worktree.clone(),
        state,
        note,
    }
}

/// The state a worktree is in, and the sentence that explains it.
///
/// `done` covers both ways of being finished, and the note is what says which
/// one: a squash or rebase merge leaves the work in `HEAD` under commits the
/// branch never had, so the reason to believe it is not the reason a plain
/// merge gives, and the row should not claim otherwise.
fn classify(dirty: bool, merged: Merged, track: Tracking) -> (State, String) {
    if dirty {
        return (
            State::Dirty,
            "uncommitted changes would be lost".to_string(),
        );
    }
    let finished = match merged {
        Merged::Commits => Some("merged into HEAD, nothing uncommitted"),
        Merged::Changes => Some("squash or rebase merged into HEAD, nothing uncommitted"),
        Merged::New | Merged::No => None,
    };
    if let Some(note) = finished {
        return (State::Done, note.to_string());
    }
    if merged == Merged::New {
        return (State::Local, "new branch, no commits yet".to_string());
    }
    match track {
        Tracking::Pushed => (
            State::Pushed,
            "not merged; every commit is on its upstream".to_string(),
        ),
        Tracking::Ahead(n) => (State::Local, format!("{n} commit(s) not on its upstream")),
        Tracking::Gone => (
            State::Local,
            "its upstream is gone from the remote".to_string(),
        ),
        Tracking::Untracked => (State::Local, "never pushed; it has no upstream".to_string()),
    }
}

/// The same rows as the picker, for a terminal that cannot draw one.
fn print_table(candidates: &[Candidate], with_branch: bool) {
    let verdicts: Vec<String> = candidates
        .iter()
        .map(|c| c.state.verdict(with_branch))
        .collect();
    let name_width = candidates
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());
    let verdict_width = verdicts
        .iter()
        .map(|v| v.chars().count())
        .max()
        .unwrap_or(0)
        .max(VERDICT_HEADER.len());

    println!("{NAME_HEADER:<name_width$}  {VERDICT_HEADER:<verdict_width$}  {NOTE_HEADER}");
    for (candidate, verdict) in candidates.iter().zip(&verdicts) {
        println!(
            "{:<name_width$}  {verdict:<verdict_width$}  {}",
            candidate.name, candidate.note,
        );
    }
}

/// Column labels, shared with the interactive screen.
pub const NAME_HEADER: &str = "WORKTREE";
pub const VERDICT_HEADER: &str = "SAFE TO REMOVE";
pub const NOTE_HEADER: &str = "NOTE";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncommitted_changes_outrank_everything() {
        // Merged and pushed, but the edits in the working tree are the only
        // thing here that a removal could destroy.
        let (state, note) = classify(true, Merged::Commits, Tracking::Pushed);
        assert_eq!(state, State::Dirty);
        assert!(note.contains("uncommitted"));
    }

    #[test]
    fn a_squash_merge_is_done_too_and_says_which_kind() {
        // The commits are unreachable from HEAD and the work is on it, which
        // is what a squash or rebase merge leaves behind. Ticking the row is
        // the whole point; the note is there to say why gwx believes it.
        let (state, note) = classify(false, Merged::Changes, Tracking::Gone);
        assert_eq!(state, State::Done);
        assert!(state.preselected());
        assert!(note.contains("squash or rebase"), "{note}");
    }

    #[test]
    fn only_merged_and_clean_is_preselected() {
        assert!(classify(false, Merged::Commits, Tracking::Pushed)
            .0
            .preselected());
        assert!(!classify(false, Merged::New, Tracking::Untracked)
            .0
            .preselected());
        for track in [Tracking::Pushed, Tracking::Ahead(2), Tracking::Gone] {
            assert!(!classify(false, Merged::No, track).0.preselected());
        }
        assert!(!classify(true, Merged::Commits, Tracking::Pushed)
            .0
            .preselected());
    }

    #[test]
    fn an_unmerged_branch_is_told_apart_by_its_upstream() {
        assert_eq!(
            classify(false, Merged::No, Tracking::Pushed).0,
            State::Pushed,
            "everything is on the remote"
        );
        assert_eq!(
            classify(false, Merged::No, Tracking::Ahead(3)).0,
            State::Local
        );
        assert_eq!(
            classify(false, Merged::No, Tracking::Untracked).0,
            State::Local
        );
        assert_eq!(classify(false, Merged::No, Tracking::Gone).0, State::Local);
    }

    #[test]
    fn the_verdict_answers_before_it_classifies() {
        assert_eq!(State::Done.verdict(false), "yes");
        assert_eq!(State::Pushed.verdict(false), "yes (pushed)");
        assert_eq!(State::Local.verdict(false), "yes (local)");
        assert_eq!(State::Dirty.verdict(false), "no (dirty)");
    }

    #[test]
    fn taking_the_branch_too_makes_local_commits_unsafe() {
        // The worktree alone loses nothing; the branch is what holds the
        // commits that are on no remote.
        assert_eq!(State::Local.verdict(true), "no (local)");
        assert_eq!(State::Pushed.verdict(true), "yes (pushed)");
        assert_eq!(State::Done.verdict(true), "yes");
    }

    #[test]
    fn the_note_says_how_many_commits_are_at_stake() {
        let (_, note) = classify(false, Merged::No, Tracking::Ahead(3));
        assert!(note.contains('3'), "{note}");
    }
}
