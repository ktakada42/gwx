//! Repository discovery and worktree name resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::git::{self, Worktree};

/// Name that always refers to the main worktree.
pub const MAIN_ALIAS: &str = "@";

pub struct Repo {
    pub cwd: PathBuf,
    /// The main worktree — all configuration paths are relative to it.
    pub main: PathBuf,
    pub config: Config,
}

impl Repo {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to read the current directory")?;
        let main = git::main_worktree(&cwd)?;
        let config = Config::merge(Config::load_global()?, Config::load(&main)?);
        Ok(Self { cwd, main, config })
    }

    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        git::list_worktrees(&self.cwd)
    }

    pub fn base_dir(&self) -> Result<PathBuf> {
        self.config.base_dir(&self.main)
    }

    /// Path a worktree for `branch` would get, following the configured layout.
    pub fn worktree_path_for(&self, branch: &str) -> Result<PathBuf> {
        Ok(self.base_dir()?.join(branch))
    }

    /// All gwx aliases configured for this repository.
    pub fn aliases(&self) -> Result<BTreeMap<String, String>> {
        git::aliases(&self.main)
    }

    /// Label shown in `gwx list`: alias when configured, otherwise path relative to
    /// the base dir when it lives there, otherwise something still recognizable.
    pub fn display_name(&self, wt: &Worktree, main: &Worktree) -> String {
        if wt.path == main.path {
            return MAIN_ALIAS.to_string();
        }
        if let Some(branch) = &wt.branch {
            if let Ok(aliases) = self.aliases() {
                for (alias, target_branch) in aliases {
                    if &target_branch == branch {
                        return alias;
                    }
                }
            }
        }
        wt.name()
    }

    /// Finds the worktree a user-supplied name refers to.
    ///
    /// `None` and `@` mean the main worktree. Otherwise the name is matched
    /// against aliases first, then branch names, then paths below the base dir,
    /// then directory names.
    pub fn resolve(&self, name: Option<&str>) -> Result<Worktree> {
        let worktrees = self.worktrees()?;
        let Some(main) = worktrees.first().cloned() else {
            bail!("no worktrees found");
        };

        let name = match name {
            None | Some(MAIN_ALIAS) => return Ok(main),
            Some(name) => name.trim_end_matches('/'),
        };

        let aliases = self.aliases().unwrap_or_default();
        if let Some(target_branch) = aliases.get(name) {
            if let Some(wt) = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(target_branch))
            {
                return Ok(wt.clone());
            }
        }

        let base = self.base_dir()?;
        let mut by_branch = Vec::new();
        let mut by_relpath = Vec::new();
        let mut by_dirname = Vec::new();

        for wt in &worktrees {
            if wt.branch.as_deref() == Some(name) {
                by_branch.push(wt);
                continue;
            }
            if wt
                .path
                .strip_prefix(&base)
                .map(|p| p == Path::new(name))
                .unwrap_or(false)
            {
                by_relpath.push(wt);
                continue;
            }
            if wt.path.file_name().map(|f| f == name).unwrap_or(false) {
                by_dirname.push(wt);
            }
        }

        for candidates in [by_branch, by_relpath, by_dirname] {
            match candidates.len() {
                0 => continue,
                1 => return Ok(candidates[0].clone()),
                _ => {
                    let paths = candidates
                        .iter()
                        .map(|w| w.path.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n  ");
                    bail!("`{name}` is ambiguous, it matches:\n  {paths}");
                }
            }
        }

        let known = worktrees
            .iter()
            .map(|w| self.display_name(w, &main))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no worktree named `{name}`. Available: {known}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::parse_worktree_list;

    fn repo() -> Repo {
        Repo {
            cwd: PathBuf::from("/home/me/repo"),
            main: PathBuf::from("/home/me/repo"),
            config: Config::default(),
        }
    }

    const PORCELAIN: &str = "\
worktree /home/me/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/me/worktrees/feature/auth
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/auth

worktree /home/me/worktrees/detached
HEAD 3333333333333333333333333333333333333333
detached
";

    #[test]
    fn parses_porcelain_output() {
        let list = parse_worktree_list(PORCELAIN);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(
            list[1].path,
            PathBuf::from("/home/me/worktrees/feature/auth")
        );
        assert!(list[2].detached);
        assert_eq!(list[2].name(), "detached");
        assert_eq!(list[0].short_head(), "1111111");
    }

    #[test]
    fn worktree_path_follows_base_dir() {
        assert_eq!(
            repo().worktree_path_for("feature/auth").unwrap(),
            PathBuf::from("/home/me/worktrees/feature/auth")
        );
    }
}
