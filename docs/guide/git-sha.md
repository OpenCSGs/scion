# Git-compatible commit SHAs

English | [简体中文](git-sha.zh-CN.md)

Scion's commit ids are 64 hex digits (`c1…`). Many external systems expect a commit id to be a 40-digit git SHA: starhub's front-end routes, cache keys and the HF SDK's `revision` parameter, for example. So that they need no changes, the server gives **every commit** a git-compatible SHA and accepts it as a revision.

## Getting one

| How | What |
|---|---|
| `QueryService.GetGitShas(repo, changeset_ids)` | A batch of up to 1,000, the results in the order of the request |
| `QueryService.GetDerived(kind = GIT_SHA1)` | One revision (a ref name, a commit id or a git SHA) |
| Event pushes and pre-land checks | `old_git_sha`, `new_git_sha` and each commit's `git_sha` (see [ref events](events.md)) |
| SDK | `Client::git_shas(&ids)`, `Client::git_sha(revision)` |

The other way round, every API that takes a revision (`ResolveRevision`, `Log`, `Diff`, `ReadFile` and so on) accepts a 40-digit git SHA.

The server declares the feature `git.sha1` in `GetServerInfo`. Older servers give SHAs only to commits imported from git.

## How it is computed

- **Commits imported from git:** the original SHA recorded at import (the extension field `git.sha1`), identical to the source repository's. Only repository admins can create commits with this field; other writers get `PERMISSION_DENIED`, and server-side rebases drop it.
- **Other commits:** git's object format, level by level:
  - files: `blob`, with mode `100644`, `100755` for executables and `120000` for symbolic links;
  - directories: `tree`, entries sorted by git's rules;
  - commits: `commit`, with the tree, the parents' git SHAs, author and committer (times to the second, time zones as `±hhmm`) and the message;
  - the commit object has one more header after committer, `scion-id <commit id>`:
    - git allows such extra headers (jj's `change-id` is one), and `git fsck --strict` passes;
    - with it, different Scion commits never share a SHA. Git's times stop at seconds and carry no change id, so without it two commits with the same content in the same second would get the same SHA.
  - `<`, `>` and newlines are removed from names and emails, since git does not allow them.

## Caching and cost

- Results go into derived data: commit → SHA, SHA → commit, tree → SHA and file → SHA. Later queries are one batched read, and forks copy these results too.
- A commit's SHA is computed the first time it is asked for, after its ancestors that were not computed yet.
  - Unchanged subdirectories have the same ids and hit the cache, so a new commit computes only the objects on its changed paths.
  - A repository's first commit reads every file once.
- With event pushes or pre-land checks configured, every ref change computes the new commits' SHAs on the way, so queries usually hit the cache.

## Limits

- After a file's content is purged (`scion admin purge`), commits not computed yet can no longer be:
  - `GetGitShas` and `GetDerived` return `FAILED_PRECONDITION`;
  - the fields in events and pre-land checks are `null`, and events are still pushed.
- Commits already computed are not affected.
- Redaction does not affect SHAs: they are computed from the original content and stay what they were before redaction.
