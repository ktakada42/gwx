//! `gwx list` — show every worktree of the repository.
//!
//! In a terminal this is the interactive picker; `--plain` and `--paths` are
//! the ways to ask for text a script can read.

use anyhow::Result;

use crate::cd_target;
use crate::cli::ListArgs;
use crate::git::Worktree;
use crate::repo::Repo;
use crate::tui::{self, Outcome};

pub fn run(args: ListArgs) -> Result<()> {
    let repo = Repo::discover()?;

    if !wants_text(&args) && tui::should_pick(&repo)? {
        return match tui::pick(&repo)? {
            Outcome::Cancelled => Ok(()),
            Outcome::Selected(path) => cd_target::request_picked(&path),
        };
    }

    let worktrees = repo.worktrees()?;

    if args.paths {
        for wt in &worktrees {
            println!("{}", wt.path.display());
        }
        return Ok(());
    }

    let Some(main) = worktrees.first() else {
        return Ok(());
    };

    let rows: Vec<Row> = worktrees
        .iter()
        .map(|wt| Row {
            current: wt.path == repo.cwd || repo.cwd.starts_with(&wt.path),
            name: repo.display_name(wt, main),
            head: wt.short_head(),
            note: note(wt),
            path: wt.path.display().to_string(),
        })
        .collect();

    let name_width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());

    // The picker labels its columns, and this is the same table without the
    // terminal to draw it in. `--no-header` is for the pipe on the other end.
    if !args.no_header {
        println!("  {NAME_HEADER:<name_width$}  {HEAD_HEADER}  {PATH_HEADER}");
    }

    for row in &rows {
        let marker = if row.current { '*' } else { ' ' };
        println!(
            "{marker} {:<name_width$}  {}  {}{}",
            row.name, row.head, row.path, row.note
        );
    }
    Ok(())
}

/// Whether the flags asked for text rather than the picker.
///
/// `--no-header` belongs here for the same reason as the other two: it shapes
/// a printed table, and the picker draws a header no flag of `list` controls.
/// Left out, it would be accepted and then silently ignored.
fn wants_text(args: &ListArgs) -> bool {
    args.paths || args.plain || args.no_header
}

/// Column labels, the same words the picker uses.
const NAME_HEADER: &str = "WORKTREE";
const HEAD_HEADER: &str = "HEAD   ";
const PATH_HEADER: &str = "PATH";

struct Row {
    current: bool,
    name: String,
    head: String,
    note: String,
    path: String,
}

fn note(wt: &Worktree) -> String {
    let mut notes = Vec::new();
    if wt.bare {
        notes.push("bare");
    }
    if wt.detached {
        notes.push("detached");
    }
    if wt.locked {
        notes.push("locked");
    }
    if wt.prunable {
        notes.push("broken");
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use std::path::PathBuf;

    fn list_args(args: &[&str]) -> ListArgs {
        match Cli::parse_from(args).command {
            Command::List(args) => args,
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn asking_for_a_shaped_table_asks_for_the_table() {
        // Each of these means "print text", so none of them may be accepted
        // and then ignored in favour of the picker.
        for flag in ["--plain", "--paths", "--no-header"] {
            assert!(
                wants_text(&list_args(&["gwx", "list", flag])),
                "{flag} would have opened the picker"
            );
        }
        assert!(!wants_text(&list_args(&["gwx", "list"])));
    }

    #[test]
    fn no_header_cannot_be_combined_with_paths() {
        // There are no columns to label in the path listing.
        assert!(Cli::try_parse_from(["gwx", "list", "--paths", "--no-header"]).is_err());
    }

    #[test]
    fn notes_describe_worktree_state() {
        let mut wt = Worktree {
            path: PathBuf::from("/wt"),
            head: None,
            branch: None,
            bare: false,
            detached: true,
            locked: true,
            prunable: false,
        };
        assert_eq!(note(&wt), " (detached, locked)");
        wt.detached = false;
        wt.locked = false;
        assert_eq!(note(&wt), "");
    }
}
