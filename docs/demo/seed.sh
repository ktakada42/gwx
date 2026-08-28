#!/usr/bin/env bash
# Give the demos a repository that already has worktrees, for the recordings
# that show what a working day looks like rather than the first minute of one.
#
# The states are picked so the picker's STATUS column has something to say:
# one worktree with uncommitted work, one merged into main, one newly created,
# and one with commits of its own.
set -euo pipefail

cd "$HOME/repo"

# Create a merged branch before adding the worktrees so the picker has a
# `merged` row to show alongside `new` and `dirty`.
git checkout -q -b fix/typo
git commit -q --allow-empty -m "Fix typo in index.js"
git checkout -q main
git merge -q --no-ff fix/typo -m "Merge branch 'fix/typo'"

# Reset feature/billing to main so it is treated as a newly created branch
git branch -f feature/billing main

for branch in feature/auth feature/billing fix/typo hotfix/login; do
    gwx add "$branch" >/dev/null 2>&1
done

echo '// TODO: refresh the token' >>"$HOME/worktrees/feature/auth/src/index.js"
git -C "$HOME/worktrees/hotfix/login" commit -q --allow-empty -m "Fix the login redirect"

