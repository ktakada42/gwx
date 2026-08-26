# Commands

| Command | What it does |
| --- | --- |
| `gwx add <branch>` | Create a worktree for `<branch>`, creating the branch if needed |
| `gwx list` (`ls`) | Pick a worktree interactively; `--plain` or `--paths` for text |
| `gwx cd [<name>]` | Move into a worktree; with no argument, to the main one |
| `gwx remove <name>` (`rm`) | Remove a worktree, optionally with its branch |
| `gwx clean` | Review the worktrees you are done with and remove the ones you pick |
| `gwx init` | Write a `.gwx.toml` template |
| `gwx shell-init <shell>` | Print the `cd` function and the completion hookup |
| `gwx completion <shell>` | Print the completion hookup only |

Every command also has a man page: `man gwx`, `man gwx-add`, and so on.

## How a name is resolved

`<name>` is matched against branch names first, then paths below `base_dir`,
then directory names — so `gwx cd feature/auth` and `gwx cd auth` both work
when they are unambiguous.

A bare `gwx cd` goes to the main worktree, the way a bare `cd` takes you home.
`gwx cd @` says the same thing explicitly. To choose from a list instead, use
[`gwx list`](picker.md).

## `gwx add`

```
gwx add <branch> [--from <commit-ish>] [--path <path>]
                 [--no-create] [--no-hooks] [--force] [--cd] [--quiet]
```

The branch is resolved in this order:

1. **Local branch exists** → check it out.
2. **Exactly one remote branch matches** → create a local branch tracking it
   (`origin/feature/auth` → `feature/auth`).
3. **Otherwise** → create the branch from `HEAD`, or from `--from` when given.

Use `--no-create` to fail instead of creating a branch, `--path` to override
the generated path, and `--quiet` to print only the resulting path — handy in
scripts:

```bash
cd "$(gwx add feature/auth --quiet)"
```

`--no-hooks` skips the `pre_create` and `post_create` hooks. See
[Hooks](hooks.md).

### `--cd`: land in the worktree you just made

`gwx add --cd` leaves your shell inside the new worktree instead of where you
typed the command:

```console
~/repo $ gwx add feature/auth --cd
Created branch `feature/auth` from `HEAD`
Running post_create hooks...
  [1/1] npm install
Worktree ready.
~/worktrees/feature/auth $
```

The move happens **after** `post_create`, so you arrive in a worktree that is
set up rather than one that is still installing. If a hook fails, the move is
called off: the worktree is left for you to look at, and a changed prompt would
only hide the failure.

The path is still printed, so `--quiet` keeps working for scripts whether or
not `--cd` is given.

> [!NOTE]
> **`--cd` needs the shell function**, like `gwx cd` and the picker do — a
> process cannot move its parent shell on its own. The function only grew to
> cover `add` when `--cd` arrived, so a shell that has been open since before
> then knows nothing about it. gwx says so rather than doing nothing — start a
> new shell, or re-run the `eval` from
> [the README](https://github.com/ktakada42/gwx#shell-integration).

## `gwx remove`

```
gwx remove <name> [--with-branch] [--force] [--no-hooks]
```

Refuses to delete the main worktree, the worktree you are standing in, or one
with uncommitted changes. `--with-branch` also deletes the branch, but only
when its work is in `HEAD` of the main worktree — either merged there, or
[squash or rebase merged](clean.md#a-squash-merge-counts-as-merged);
`--force` overrides both checks. `--no-hooks` skips the `pre_remove` and `post_remove` hooks, which is
the way out when a hook itself is what stands between you and a stale
worktree.

A worktree that has lost its `.git` — deleted by hand, or left behind by a
move that did not finish — shows up as `broken`, and `git worktree remove`
refuses it outright. `--force` is what removes one: gwx deletes the directory
itself and lets git drop the record afterwards. It takes a `--force` because a
broken checkout cannot be asked whether anything in it was left uncommitted.

## `gwx clean`

```
gwx clean [--with-branch] [--force] [--no-hooks]
```

Lists every removable worktree with what removing it would cost, ticks the
ones that are merged and clean, and removes what you confirm. See
[`gwx clean`](clean.md) for the states and the keys.

## `gwx list`

Opens [the picker](picker.md) when there is a terminal to draw on and more
than one worktree to choose from. Otherwise it prints a table.

- `--plain` prints the table even in a terminal, for reading or piping.
- `--no-header` prints it without the column labels, which is what a pipe
  wants. It implies `--plain`, since the picker's header is not a flag's to
  remove.
- `--paths` prints one absolute path per line, and nothing else.

```console
$ gwx list --plain
  WORKTREE         HEAD     PATH
* @                a1b2c3d  /home/me/repo
  feature/auth     a1b2c3d  /home/me/worktrees/feature/auth
```

### Why the table and the picker differ

Both label their columns the same way, and the third column is not the same
one:

| | Third column |
| --- | --- |
| `gwx list --plain` | `PATH` |
| The picker | `STATUS` — `dirty`, `merged` |

The picker leaves the path out on purpose: gwx derives it from `base_dir` and
the branch name, so it repeated what the first column already said, and the
space was better spent on whether the worktree is safe to remove.

The table keeps it, and does not show `STATUS`, for a reason that is about
cost. Working the status out takes a `git status` per worktree, one
`git branch --merged` for the repository, and a `git merge-tree` for each
branch that call did not vouch for. Which of those dominates depends on the
shape of the repository: the merged check grows with the number of local
branches, and `git status` grows with the size of the working tree. On a
monorepo the second wins outright — hundreds of milliseconds per worktree,
against tens for everything else put together.

The picker can afford it because it fills the column in after the list is
already on screen, and asks about every worktree at once rather than one after
another; a table printed in one go would have to wait for all of it, turning a
listing that returns in milliseconds into one that does not. If you want that
judgement, [`gwx clean`](clean.md) answers a sharper version of it, and pays
for it the same way.

## `gwx shell-init` and `gwx completion`

`shell-init` prints the shell function that makes `gwx cd`, `gwx add --cd` and
the picker able to change your shell's directory, plus the completion hookup. `completion`
prints the completion hookup on its own, for people who do not want the `cd`
function.

Completion is computed by `gwx` itself while you type, so it knows about your
repository:

```console
$ gwx cd <TAB>
@                    -- main worktree — /home/me/repo
feature/auth         -- /home/me/worktrees/feature/auth

$ gwx add <TAB>
hotfix/login         -- local branch
release/2.1          -- origin/release/2.1

$ gwx remove <TAB>   # every worktree except the main one
$ gwx add x --from <TAB>   # branches, tags and remote-tracking branches
```

`gwx add` only offers branches that do not have a worktree yet — the ones it
would actually accept — and remote-only branches under the short name you would
type. Outside a repository nothing is offered instead of an error.
