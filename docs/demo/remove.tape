# `gwx remove`: the worktree and, with --with-branch, the branch it was for —
# so finishing a task takes one command too.

Source common.tape

Output remove.gif

# Fits the two listings and the removal in between, with nothing left over.
Set Height 580

Hide
Type "source /demo/env.sh" Enter
Type "bash /demo/seed.sh && clear" Enter
Show

Sleep 1s
Type "gwx list --plain"
Sleep 400ms
Enter
Sleep 2.5s

Type "gwx remove feature/billing --with-branch"
Sleep 800ms
Enter
Sleep 2.5s

Type "gwx list --plain"
Sleep 400ms
Enter
Sleep 3.5s
