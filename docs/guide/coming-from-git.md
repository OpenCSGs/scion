# Coming from git

`scn` keeps git's spelling for arguments (`HEAD~1`, `A..B`, `-- PATH`, `-am`, `branch -d`). It uses its own command names only where Scion's model really differs from git's. This page maps one to the other.

## What is different in the model

- **No staging area.** `scn commit` takes every local change. New, deleted and renamed files are found by scanning the working tree; there is no `add`, `rm` or `mv`.
- **No local history.** A working copy holds one snapshot. History lives on the server, so `log`, `blame`, `show` and `diff REV` are server queries, and `cat`, `ls` and `archive` work without a working copy.
- **Committing is not publishing.**
  - `scn commit` creates a commit on the server that is not on any branch yet.
  - `scn land` publishes it, like `git push`. If the branch moved, the server rebases the commit, and it lands exactly once.
- **One pending commit.** A working copy has at most one commit that is committed but not landed. There are no local branches or local commit stacks.
- **No remotes.** A working copy belongs to one server and one repository.

## Command map

| git | scn | Notes |
|---|---|---|
| `git init` | `scn repo create NAME` | Repositories live on the server |
| `git clone URL [DIR]` | `scn clone REPO [DIR] [--ref B]` | Downloads the snapshot, not the history |
| `git clone --sparse` + `git sparse-checkout set` | `scn clone REPO --sparse src --sparse docs`, `scn sparse set/add/rm/disable` | Directories; `--sparse a,b` works too |
| `git status` | `scn status` | `A` / `M` / `D`, plus `C` for conflicts |
| `git add`, `git rm`, `git mv` | not needed | Just change the files |
| `git commit -m MSG`, `-am MSG` | `scn commit -m MSG`, `-am MSG` | `-a` is accepted and changes nothing. The author comes from `--author`, then `scn config`, then git's `user.name` / `user.email` |
| `git commit --amend` | `scn commit --amend` | Only the pending commit |
| `git reset --soft HEAD~1` | `scn uncommit` | Drops the pending commit and keeps its changes |
| `git restore PATH`, `git checkout -- PATH` | `scn restore PATH` | `.` for everything; added files are kept (no `git clean`) |
| `git push` | `scn land` | The server rebases when the branch moved |
| `git pull --rebase` | `scn sync` | Moves to the branch tip and rebases the pending commit; no separate `fetch` |
| `git branch` | `scn branch` (or `scn branch ls`) | |
| `git branch NAME [REV]` | `scn branch NAME [REV]` (or `scn branch create`) | Branches are server refs. A branch named `ls`, `create` or `rm` needs the `create` form |
| `git branch -d NAME` / `-D` | `scn branch -d NAME` / `-D` (or `scn branch rm`) | |
| `git tag`, `git tag NAME [REV]`, `git tag -d NAME` | `scn tag`, `scn tag NAME [REV]`, `scn tag -d NAME` | Lightweight tags only (no messages). Stored as refs `tags/NAME` that the server never moves. A tag name works as a revision (`scn log v1.0`) unless a branch has the same name |
| `git switch B`, `git switch -c B` | `scn switch B`, `scn switch -c B` | Refuses with a pending commit or modified files; added files are carried over |
| `git merge B` | `scn merge B` | Makes a pending merge commit; `scn land` publishes it |
| `git merge --abort` | `scn merge --abort` | |
| `git add` after fixing a conflict | `scn resolve PATH`, `--ours` / `--theirs` | Refuses leftover markers unless `--force`; no paths lists conflicts |
| `git rebase --continue`, `git merge --continue` | `scn commit` | `-m` is optional; it keeps the original message |
| `git log [REV] [-n N] [--oneline] [-- PATH]` | `scn log [REV] [-n N] [--oneline] [-- PATH]` | Always one line per commit; `--path PATH` also works |
| `git show [REV] [-- PATH]` | `scn show [REV] [--name-only] [-- PATH]` | |
| `git diff`, `git diff A B`, `A..B`, `A...B` | `scn diff`, `scn diff A B`, `A..B`, `A...B` | `--name-only`, `-- PATH` |
| `git blame [REV] -- PATH` | `scn blame [REV:]PATH` | |
| `git show REV:PATH` | `scn cat REV:PATH` | No working copy needed |
| `git ls-tree REV DIR` | `scn ls REV:DIR` | |
| `git archive` | `scn archive [REV:][DIR] --format tar.gz\|zip\|tar [--prefix P] [-o FILE]` | |
| `git config user.name` / `user.email` | `scn config set user.name` / `user.email` | The only settings so far |
| `.gitignore` | `.gitignore` and `.scionignore` | Same syntax; `.scionignore` wins in the same directory |
| `git fast-import` | `scn import git PATH --repo R` | Keeps the original git SHAs |

## Transitional git spellings

To ease the move, these git commands also work. Each runs the scn command on the right and prints a hint on stderr naming it, for example:

```
hint: `scn push` is a transitional git-style spelling; use `scn land` instead
```

They are not listed in `scn --help` and may be removed later, so scripts should use the scn names.

| You type | scn runs |
|---|---|
| `scn push` | `scn land` |
| `scn pull [--rebase]` | `scn sync` |
| `scn checkout BRANCH`, `scn checkout -b NAME` | `scn switch BRANCH`, `scn switch -c NAME` |
| `scn checkout -- PATH`, `scn checkout PATH` (an existing path) | `scn restore PATH` |
| `scn reset --soft [HEAD~1]` | `scn uncommit` |
| `scn reset --hard [HEAD]` | `scn restore .` |
| `scn rebase --continue`, `scn merge --continue` | `scn commit` |
| `scn add ...` | nothing: there is no staging area |
| `scn rm [-r] PATH...`, `scn mv FROM TO` | deletes / moves the files; the next `scn commit` picks it up |

Other forms, like `reset --hard HEAD~3` or `rebase --abort`, fail with a message that explains the scn way.

## Revisions

| Form | Meaning |
|---|---|
| `main`, `feature/x` | A branch |
| `v1.0`, `tags/v1.0` | A tag |
| `c1…` | A Scion commit id |
| 40 hex digits | A git SHA. Every commit has one; see [git-sha.md](git-sha.md) |
| `HEAD`, `@` | The working copy's current commit: the pending one if any, else the synced one |
| `REV~N` | The N-th first-parent ancestor (`~` alone means `~1`) |
| `REV^`, `REV^N` | The first or N-th parent (`^0` is the commit itself) |

Suffixes combine as in git, for example `main~2^2`. A branch whose name literally contains `~` or `^` takes precedence over the suffix reading.

## Not available yet

- **Working copy**: checking out an old revision, `revert`, `cherry-pick`, `stash`, `clean`, interactive rebase, partial commits (`add -p`).
- **Queries**: annotated tags, `grep`, `bisect`, `reflog`.
- **Collaboration**: `remote`, `fetch`. git clients cannot talk to a Scion repository yet; that is the git gateway in the [roadmap](../design/roadmap.md).
- **Rebase conflicts**: a sync that stops on conflicts has no `--abort`. It drops the pending commit, and its changes stay as local changes on top of the new tip.
