# `gwx clean`

Worktrees pile up. `gwx clean` shows the ones you could remove, says what each
would cost, and removes the ones you tick.

```
Select worktrees to remove

      WORKTREE         SAFE TO REMOVE  NOTE
> [x] feature/auth     yes             merged into HEAD, nothing uncommitted
  [ ] feature/billing  yes (pushed)    not merged; every commit is on its upstream
  [ ] hotfix/login     yes (local)     2 commit(s) not on its upstream
  [ ] wip/refactor     no (dirty)      uncommitted changes would be lost

1 of 4 selected
 up/down  move   space  toggle   enter  remove   esc  cancel
```

```
gwx clean [--with-branch] [--force] [--no-hooks]
```

## What the column says

**Safe** means removing it destroys nothing. That is a narrower question than
whether the work is finished, and the two answers differ: three of the four
states are safe to remove, and only one of them is ticked for you. The state
in brackets is what the verdict rests on, because "no" on its own does not say
what to do about it.

With `--with-branch` the question changes, and so does the answer: a `local`
branch takes its commits with it, so it becomes `no (local)`.

## What the states mean

The classification rests on one fact: **removing a worktree does not remove
commits.** The branch keeps them. So the only thing a removal can destroy is
work that was never committed — everything else here is about whether the work
is *finished*, which is why only `done` is ticked for you.

| State | Meaning | Removing the worktree | Also removing the branch |
| --- | --- | --- | --- |
| `done` | The work is in the main worktree's HEAD (or, with [`use_gh_merge_base`](configuration.md#use_gh_merge_base), its target branch), nothing uncommitted | Loses nothing | Loses nothing; the work is in HEAD |
| `pushed` | Not merged, but every commit is on its upstream | Loses nothing | Loses nothing; the commits are on the remote |
| `local` | Commits that are on no remote, or no upstream at all | Loses nothing; the branch keeps the commits | **Loses those commits** |
| `dirty` | Uncommitted changes | **Loses them** | Same |

A branch whose upstream has been deleted from the remote counts as `local`: the
commits may be on no remote any more, so gwx treats them as yours alone. In a
"squash and merge" workflow that branch is usually `done` instead, for the
reason below.

## A squash merge counts as merged

`git branch --merged` asks whether a branch's tip is reachable from `HEAD`. A
squash merge writes the branch's cumulative diff as one new commit and leaves
the commits it was made of unreferenced, so the branch is finished in every
sense that matters and invisible to that question. A rebase merge does the same
thing with new hashes. In a GitHub "Squash and merge" workflow the remote
branch is deleted at merge time too, which is how the worktrees that are safest
to remove ended up labelled `local`.

So `done` covers both, and the note says which one gwx is looking at:

```
feature/auth     yes             merged into HEAD, nothing uncommitted
feature/billing  yes             squash or rebase merged into HEAD, nothing uncommitted
```

The second is a content test: gwx merges the branch into `HEAD` in memory and
asks whether the result differs from `HEAD`. Two things follow from that.

- Work that was merged and then reverted is **not** `done`. Putting it back is
  a change, so the trees differ and the branch keeps whatever state its
  upstream gives it.
- A branch that changed nothing at all is not `done` either. It merges into
  anything without effect, which proves nothing about where its commits went.

`git branch -d` runs the same ancestry test that missed the squash merge, so
under `--with-branch` gwx deletes such a branch with `-D`. That is gwx
answering for the deletion rather than git, and it is why the content test is
the strict one described above.

It needs git 2.38 or newer. Older git falls back to the ancestry test alone,
which is what gwx did before.

## Keys

| Key | Action |
| --- | --- |
| <kbd>↑</kbd> <kbd>↓</kbd> (or <kbd>Ctrl</kbd>+<kbd>p</kbd> / <kbd>n</kbd>) | Move the cursor |
| <kbd>Space</kbd> | Tick or untick the row |
| <kbd>Enter</kbd> | Remove what is ticked |
| <kbd>Esc</kbd> or <kbd>Ctrl</kbd>+<kbd>c</kbd> | Leave without removing anything |

## Flags

- `--with-branch` deletes each removed worktree's branch too, and only when it
  is merged — the same rule [`gwx remove`](commands.md#gwx-remove) follows.
- `--force` allows removing a `dirty` worktree, and deleting an unmerged
  branch alongside `--with-branch`. Without it, a ticked `dirty` row is
  skipped and said so.
- `--no-hooks` skips `pre_remove` and `post_remove` for the whole run.

Removals happen one at a time and a failure does not stop the rest: you asked
for several removals, not for a transaction. Each one is reported as it goes.

The main worktree and the one you are standing in never appear in the list, the
same way `gwx remove` refuses them.

## Without a terminal

In a script, a pipe or CI there is nobody to tick the boxes, so `gwx clean`
prints the table and removes nothing:

```console
$ gwx clean | cat
WORKTREE      SAFE TO REMOVE  NOTE
feature/auth  yes             merged into HEAD, nothing uncommitted
wip/refactor  no (dirty)      uncommitted changes would be lost
gwx clean needs a terminal to choose in; nothing was removed.
```
