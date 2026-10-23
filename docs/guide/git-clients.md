# Using git with Scion repositories

English | [简体中文](git-clients.zh-CN.md)

The server speaks git's smart HTTP protocol at the same address and URL as `scn`, so the git CLI, CI systems and IDEs can clone and fetch a Scion repository. Pushing with git comes later; until then, push with `scn`. How the gateway works is in the [design](../design/git-gateway.md).

- [Clone and fetch](#clone-and-fetch)
- [Credentials](#credentials)
- [SHAs](#shas)
- [Not available yet](#not-available-yet)
- [Turning it off](#turning-it-off)

## Clone and fetch

A repository's git URL is its `scn` URL; a `.git` suffix works too:

```sh
git clone http://scion.example.com:7080/agentlab/app
cd app
git pull                        # fast-forwards to what landed since
git fetch origin feature        # any branch
git ls-remote origin            # HEAD, refs/heads/*, refs/tags/*
```

- Branches appear as `refs/heads/<name>`, tags (`tags/<name>` in Scion) as `refs/tags/<name>`, and `HEAD` points at the repository's default branch.
- git 2.x clients work, with protocol v2 (git's default since 2.26) and the older v0.
- A clone receives every commit of the branches it fetches, not just the snapshot `scn clone` downloads.

## Credentials

The gateway takes the same tokens as `scn`, as the password of HTTP Basic authentication with any user name:

```sh
git clone http://git:$SCION_TOKEN@scion.example.com:7080/agentlab/app
```

Or let a credential helper keep the token (`git config --global credential.helper store`, then enter any user name and the token when git asks). `Authorization: Bearer <token>` works too, for tools that send headers (`git -c http.extraHeader="Authorization: Bearer $SCION_TOKEN" clone …`).

- Without credentials, git asks for them when the repository is not readable anonymously.
- A token that cannot read the repository gets `403`.
- The same policy applies as for `scn` (see [authentication](auth.md)): fetching needs `read` on the repository.

## SHAs

Every commit has one git SHA, the one `GetGitShas` and the SDK's `git_shas` report ([git SHAs](git-sha.md)):

- **Commits imported from git** keep their original SHAs, so a clone has exactly the source repository's objects. Commits imported with signatures (`gpgsig`) or submodules cannot be rebuilt yet (see below).
- **Commits made with `scn`** get a SHA computed from their content, with a `scion-id` header in the commit object. `git fsck --strict` accepts it.

## Not available yet

| What | What happens |
|---|---|
| `git push` | Refused with `pushing with git is not supported yet; push with scn` |
| Shallow clones (`--depth`) and partial clones (`--filter`) | git refuses them, since the server does not offer them |
| Signed commits or submodules imported from git | The fetch fails with `commit … was imported from git without its raw object and cannot be rebuilt; re-import it or use scn` |
| History containing a [redacted](redaction.md) file | The fetch fails: the file's SHA covers content nobody may read any more |
| SSH URLs | Use HTTP(S) |

## Turning it off

`scion server --no-git` (or `SCION_NO_GIT=1`) stops serving git; the gRPC API is unaffected.
