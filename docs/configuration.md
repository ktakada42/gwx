# Configuration

`gwx` reads `.gwx.toml` from the **main worktree** — the original clone — no
matter which worktree you run it from, so hook paths always mean the same
thing. Run `gwx init` to get a commented template. Everything is optional.

There is a second file, in the same format, for what is yours rather than the
project's: `~/.config/gwx/config.toml` (or `$XDG_CONFIG_HOME/gwx/config.toml`).
See [Two files](#two-files) for how the two combine.

```toml
version = "1"

[defaults]
# Where worktrees are created, relative to the main worktree.
base_dir = "../worktrees"
# Count a branch as merged once its work is on its gh-merge-base too.
use_gh_merge_base = false

# Runs before the worktree exists, in the main worktree. Commands only.
[[hooks.pre_create]]
type = "command"
command = "git fetch --prune"

# Runs inside the new worktree, in order.
[[hooks.post_create]]
type = "copy"
from = ".env"          # relative to the main worktree
to = ".env"            # relative to the new worktree; defaults to `from`

# Share one node_modules with the main worktree. On macOS a `copy` is a
# clone and costs about the same — see hooks.md for which to pick.
[[hooks.post_create]]
type = "symlink"
from = "node_modules"

[[hooks.post_create]]
type = "command"
command = "npm install"
work_dir = "."         # relative to the new worktree
env = { NODE_ENV = "development" }

# Runs inside the worktree while it is still there. Commands only.
# A failure calls the removal off.
[[hooks.pre_remove]]
type = "command"
command = "docker compose down"

# Runs once the worktree is gone, in the main worktree. Commands only.
[[hooks.post_remove]]
type = "command"
command = "rm -rf \"$GWX_MAIN_WORKTREE/.cache/$GWX_WORKTREE_NAME\""
```

What the hooks themselves can do is in [Hooks](hooks.md).

## A worked example

The keys above, as one repository would actually use them — a web application
with a database, secrets git does not track, and a `node_modules` too large to
copy per branch:

```toml
version = "1"

[defaults]
base_dir = "../worktrees"

# Start from refs that are not a week old, so `gwx add release/2.1` finds the
# branch a colleague pushed this morning. Runs in the main worktree, where the
# remotes are configured.
[[hooks.pre_create]]
type = "command"
command = "git fetch --prune"

# The secrets git does not track. Without this the first `npm run dev` in a new
# worktree fails on a missing DATABASE_URL, which is a confusing way to find
# out you are somewhere else.
[[hooks.post_create]]
type = "copy"
from = ".env"

[[hooks.post_create]]
type = "copy"
from = ".env.local"

# One node_modules for every worktree. On Linux this is what keeps a 400MB
# directory from being copied per branch; on macOS a `copy` clones and costs
# about the same, so prefer that if the worktrees install different versions.
[[hooks.post_create]]
type = "symlink"
from = "node_modules"

# Bring the new checkout up to the branch's own lockfile. It is a no-op when
# the dependencies match the shared node_modules, and the whole point when the
# branch is the one that changed them.
[[hooks.post_create]]
type = "command"
command = "npm install --no-audit --no-fund"
env = { NODE_ENV = "development" }

# A database per worktree, named after the branch, so migrations on one branch
# cannot break the review you are doing on another. The slashes in a branch
# name are not allowed in a compose project name, hence the `tr`.
[[hooks.post_create]]
type = "command"
command = "docker compose -p \"gwx-$(echo \"$GWX_WORKTREE_NAME\" | tr / -)\" up -d db"

# Stop what the worktree started. This runs inside the worktree while it still
# exists, which is what `docker compose` needs to find its compose file — and
# a failure here calls the removal off, so the containers are never orphaned
# by a directory disappearing from under them.
[[hooks.pre_remove]]
type = "command"
command = "docker compose -p \"gwx-$(echo \"$GWX_WORKTREE_NAME\" | tr / -)\" down --volumes"

# Whatever was keyed to the worktree's path rather than living inside it. The
# directory is gone by now, so this runs in the main worktree and is handed the
# path that was removed.
[[hooks.post_remove]]
type = "command"
command = "rm -rf \"$GWX_MAIN_WORKTREE/.cache/playwright/$GWX_WORKTREE_NAME\""
```

Two things in there are worth stealing. The database is named after the
worktree, so a migration on one branch cannot break the review running on
another; and the hook that starts it has a `pre_remove` that stops it, which
is the pairing that keeps containers from outliving the directory they were
started for.


## `base_dir`

With `base_dir = "../worktrees"`, a repository at `/home/me/repo` puts the
worktree for `feature/auth` at `/home/me/worktrees/feature/auth`. Slashes in
branch names become directories. An absolute `base_dir` is used as-is.

Two things are expanded before the path is used:

| | |
| --- | --- |
| `~` | Your home directory, at the start of the path only (`~/worktrees`) |
| `{repo}` | The directory name of the main worktree |

Both exist for the user-wide config, where one line has to work in every
repository:

```toml
# ~/.config/gwx/config.toml
[defaults]
base_dir = "~/worktrees/{repo}"
```

That puts this repository's `feature/auth` at `~/worktrees/gwx/feature/auth`,
and the next repository's somewhere it cannot collide with. gwx never runs the
value through a shell, so anything else in braces, and `~user` for somebody
else's home, is an error rather than a directory with a surprising name.

## `use_gh_merge_base`

`remove --with-branch` and `clean` count a branch as merged when its work is in
`HEAD` of the main worktree. A branch cut from a feature branch and merged back
into it is not, until that feature branch reaches `main` — so gwx keeps it,
although its work is no less safe than if it had gone straight to `main`.

```toml
[defaults]
use_gh_merge_base = true
```

With this, a branch that `HEAD` turns down is asked the same question again
against the local branch named by `git config branch.<name>.gh-merge-base`.
That is the key `gh pr create` takes its default `--base` from, so a branch
set up for `gh` needs nothing more. Anything else can set it the same way:

```bash
git config branch.feature/child.gh-merge-base feature/parent
```

The test is the one `HEAD` gets — merged, squash or rebase merged, and not
merged and then reverted — and `clean` names the branch it found the work on:
`merged into feature/parent, nothing uncommitted`. `HEAD` is still asked first,
so turning this on only ever adds branches to the merged ones. A target with no
local branch, never fetched or already deleted, leaves the `HEAD` answer
standing on its own. Off by default; like `base_dir`, a repository that says
nothing inherits the user-wide setting.

## Two files

`.gwx.toml` in the repository is a decision the project makes: everyone who
clones it gets the same hooks, and it is meant to be committed. Where you like
your worktrees is not that kind of decision, and neither is what you personally
run in every checkout — those go in `~/.config/gwx/config.toml`, which nobody
else sees.

```toml
# ~/.config/gwx/config.toml
[defaults]
base_dir = ".worktrees"        # inside the repository, wherever you are
# or: "~/worktrees/{repo}"     # all of them together, one per repository
```

The two combine per key rather than one replacing the other:

| | Result |
| --- | --- |
| `[defaults]` | The repository wins where it says something; where it is silent, yours applies |
| Hooks | Both run. Neither replaces the other |

Hooks run in the order setting up and tearing down always take: on **create**,
yours first and the repository's second; on **remove**, the repository's first
and yours last — so what your hook set up is still there while the project's
teardown runs. `gwx` names each hook as it runs it, so the order is visible.

Two things to know before putting hooks in the user-wide file. They run in
**every** repository, including ones you have just cloned and have not read;
and a `copy` hook naming a file that a repository does not have is a failure,
not a skip. `--no-hooks` on `add` and `remove` is the way past a hook that is
in your way.
