# Repositories and branches

English | [简体中文](repos.zh-CN.md)

Repositories and branches are created and managed online, without restarting the server. The APIs are `RepoService` and `RefService`'s `CreateRef`, `UpdateRef` and `DeleteRef`; see [`proto/scion/v1/repo.proto`](../../proto/scion/v1/repo.proto). The commands are `scn repo` and `scn branch`, and all of them take `--json`.

## Repositories

```sh
scn repo create opencsg/qwen --description "Qwen fine-tuning" [--default-branch main]
scn repo ls [opencsg/] [--limit 100]       # only repositories you can read, by name
scn repo info opencsg/qwen
scn repo set opencsg/qwen --default-branch dev --description "…"
scn repo fork opencsg/qwen alice/qwen      # copies all history and branches
scn repo rm opencsg/qwen                   # delete; restorable during a grace period
scn repo ls --deleted                      # deleted repositories not purged yet
scn repo restore opencsg/qwen
```

A repository name is one or more segments of `[A-Za-z0-9._-]+` separated by `/` (as CSGHub's `namespace/name`), at most 256 bytes. `scion server --repo NAME` still creates a repository at start.

### Permissions

| Operation | Permission needed |
|---|---|
| Create, change, delete, restore | `admin` on the repository's name (repository wildcards in the ACL apply: `repos = ["alice/*"]` lets alice create repositories in their own namespace) |
| Show properties, list | `read`; a listing returns only repositories you can read |
| Show deleted repositories | `admin` |
| Fork | `read` on the source and `admin` on the target's name |

### Deleting and purging

- **Soft delete:** a deleted repository can no longer be found by name; reads and writes return `REPO_NOT_FOUND`, and the data stays.
- **Grace period:** set by `scion server --repo-delete-grace` (24 hours by default, at least 2), during which `scn repo restore` brings it back.
- **The name is taken meanwhile:** creating a repository with the same name during the grace period returns `ALREADY_EXISTS` / `REPO_DELETED`.
- **Purge:** after the grace period, the server's janitor deletes all the repository's metadata in batches (refs, commit records, object index, derived data, redactions and so on); only then can the name be used again.
  - Once a purge starts (with its first batch), the repository can no longer be restored: `scn repo restore` returns `FAILED_PRECONDITION`.
  - Restores and purges never interleave: each batch locks the repository's row in one transaction and checks it is still deleted before deleting.
- **Object bytes:** shared between repositories and not deleted by a purge.
- **Why the grace period has a minimum:** the server's commit record cache, the regional cache and the daemon's visible sets cache by repository name for up to an hour. The name becomes free only after that, so a new repository with the same name never sees the old one's data.

### Forks

- **What is copied:** in one transaction, the source's properties, object index, commit records, derived data, path history, every branch and the active redactions (files redacted in the source cannot be read in the fork either).
- **What is not:** land records and request results; lands in the fork count from zero.
- **Time:** proportional to the source's numbers of commits and files, all inside the database; object bytes are not copied. Asynchronous forks of very large repositories are not implemented yet.

## Branches

```sh
scn branch ls                 # * marks the default branch
scn branch create dev [REV]   # REV defaults to the ref being tracked
scn branch rm dev
```

Outside a working copy, pass `--repo`.

- **Permissions:** creating, moving and deleting a branch need `land` on the ref, as landing does. ACL rules limited to refs apply.
- **`UpdateRef`:** points a branch at a commit directly, fast-forward or not, like a force push. To move a branch forward day to day, use `scn land` (server-side rebase, batching, exactly once).
- **Default branch:** cannot be deleted; switch the default with `scn repo set --default-branch` first.
- **Versions:** a branch recreated after deletion starts again at version 1; its earlier version history (the `ref_log` audit records) is purged with the deletion.
- **WatchRefs:** does not push branch deletions.
