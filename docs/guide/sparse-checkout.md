# Sparse checkout

English | [简体中文](sparse-checkout.zh-CN.md)

In a large repository, a job usually needs only a few directories. A sparse checkout writes only those to disk:

```sh
scn clone mono --sparse services/api/,libs/common/   # comma-separated; the trailing / is optional
```

The scope holds:

- the files at the repository's root (always checked out);
- everything under the listed directories.

Files in the parents of a listed directory are **not** checked out: `--sparse a/b` does not include `a/x.txt`.

## How it works

Scion's read protocol fetches objects by id, and the client reads trees level by level. A sparse checkout needs no support from the server: the walk does not enter subdirectories outside the scope, so their trees and files are never requested and never enter the local daemon's cache. A clone's time and traffic depend only on what is in scope.

A commit sends only the changed paths, and the server builds the new trees from the parent's, so everything outside the scope stays as it is.

## Changing the scope

```sh
scn sparse ls                 # list the scope
scn sparse add tools/         # add a directory and check it out
scn sparse rm libs/common     # remove a directory and delete its files
scn sparse set docs/ web/     # set the scope (a full working copy becomes sparse)
scn sparse disable            # back to a full checkout
```

- **Uncommitted changes:** commit before changing the scope. Commits not landed yet are fine: the checked-out revision is that commit.
- **Local files outside the old scope:** judged against the new scope.
  - Still outside it: an error that lists them.
  - Inside it, with the same content as the server's at that path: taken over as they are.
  - Inside it, with different content: an error; local files are never overwritten.
  - Not on the server: kept as new files, to commit as usual.
- **Nested directories:** a directory inside another entry is dropped: `a` and `a/b` keep only `a`. `rm` takes an entry of the scope, not a subdirectory of one.
- **On a full checkout:** `add` and `rm` are errors; choose directories with `set` first.

Every command takes `--json` (`scion.cli.sparse/v1`; `dirs` is null for a full checkout).

## Everyday commands

- **sync and land:** write only changes in scope. Other people's changes outside it never show up locally and do not affect sync or rebase.
- **Conflict detection:** the client's rebase compares only files in scope, which is the only place your changes can be.
- **New files outside the scope:** status, commit, land and sync stop with an error that lists them.
  - Why: the server may already have content at those paths, just not checked out, and committing would silently overwrite it.
  - What to do: move the files into the scope, or add their directory with `scn sparse add`.

The scope is kept in the `sparse` field of `.scion/state.json`.
