# The picker

`gwx list` opens a list you can move through:

```
  WORKTREE         HEAD     STATUS
* @                a1b2c3d
  feature/auth     a1b2c3d  dirty, merged
  feature/billing  a1b2c3d  merged
  hotfix           3d3cc2d

> _ type to filter                                        4 worktrees
 up/down  move   enter  cd   ctrl-d   backspace  delete   esc  cancel
```

The table starts at the top, so the header sits against the rows it labels.
The filter joins the help line at the bottom, where the things you operate
live.

The filter line carries a block cursor and, on the right, how much of the list
you are looking at — `1 of 4` once you start typing, so filtering everything
away reads as `0 of 4` rather than an unexplained blank screen.

Each row says what you need before acting on it: `dirty` for uncommitted
changes, `merged` when the branch is already in the main worktree's `HEAD`,
`broken` when the checkout has lost the `.git` that ties it to the repository.
The `STATUS` column fills in a moment after the list appears — working it out
costs a `git status` per worktree, which the list does not wait for.
The path is not shown — gwx derives it from the branch name, so it only
repeated what the first column already said.

Each part of the screen is told apart by a different attribute rather than by
shade alone: the header is bold and underlined, the selected row is highlighted
across the full width, each key in the help line sits in a reverse-video badge,
and only the hints that fade — the placeholder and the count — are dimmed. A
`*` in the first column marks the worktree you are standing in.

## Keys

| Key | Action |
| --- | --- |
| <kbd>↑</kbd> <kbd>↓</kbd> (or <kbd>Ctrl</kbd>+<kbd>p</kbd> / <kbd>n</kbd>) | Move the cursor |
| type anything | Filter by name or path |
| <kbd>Enter</kbd> | Change into the selected worktree |
| <kbd>Backspace</kbd> | Erase the filter, or remove the worktree once it is empty |
| <kbd>Ctrl</kbd>+<kbd>d</kbd> or <kbd>Delete</kbd> | Remove the worktree, whatever you have typed |
| <kbd>Esc</kbd> or <kbd>Ctrl</kbd>+<kbd>c</kbd> | Leave without moving |

<kbd>Backspace</kbd> does double duty so that the key labelled "delete" on Mac
keyboards — which sends Backspace, not Delete — can remove a worktree. The
bottom line always says which of the two it will do right now. On a narrow
terminal it drops `up/down move` first rather than cutting a word in half.

Holding <kbd>Backspace</kbd> to clear what you typed cannot run past the empty
filter into the delete dialog: the press at that boundary is swallowed, so
reaching the dialog always takes a deliberate keystroke.

Everything the picker draws is plain ASCII, so it does not depend on the font
having arrow or return glyphs.

## Removing from the picker

The confirmation dialog names what is at stake before you answer — uncommitted
changes, and whether the branch is merged:

```
Remove this worktree?

  /home/me/worktrees/feature/auth

  ! uncommitted changes will be lost
  branch `feature/auth` (merged)

[y] remove worktree   [b] remove worktree and branch   [n] cancel
```

The main worktree and the one you are standing in are refused outright, same as
`gwx remove`.

Removal hooks run here exactly as they do for `gwx remove` — a worktree brought
down by <kbd>Ctrl</kbd>+<kbd>d</kbd> is no less removed. The picker owns the
screen while they run, so it keeps what they print to itself and shows only
what a failing one said last. See [Hooks](hooks.md).

## When the picker does not open

Two cases skip it entirely: when there is no terminal to draw on (a script, a
pipe, CI) and when the repository has no worktree other than the main one.
`gwx list` then prints its table, exactly as before, so existing scripts keep
working. Ask for text in a terminal with `gwx list --plain`, or
`gwx list --paths` for one path per line.

The picker hands the directory to the shell function through a temporary file
named by `GWX_CD_FILE`, which leaves stdout free for `gwx list` to print on.
That means `gwx shell-init` changed in v1.1.0: after upgrading, start a new
shell (or re-source your rc file) before `gwx list` can move you.
