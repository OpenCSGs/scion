# A tour of scion and scn

Starting a server, then the commands of an everyday workflow. [Coming from git](coming-from-git.md) maps git commands to their scn equivalents; every command has `--help`, and `--json` for scripts.

```sh
# Option 1: docker compose with PostgreSQL + S3-compatible storage (see deploy.md)
docker compose -f deploy/compose/docker-compose.yml up -d --build

# Option 2: a server in Lite mode (SQLite + a local directory, persisted), from source
cargo run -p scion -- server --lite --data-dir ./.scion-data

# Other backends: --meta mem|sqlite://PATH|postgres://…  --blobs mem|DIR|s3://bucket/prefix
# S3 settings: SCION_S3_ENDPOINT, SCION_S3_ACCESS_KEY_ID, SCION_S3_SECRET_ACCESS_KEY, SCION_S3_REGION,
# SCION_S3_PROVIDER=auto|minio|seaweedfs|aws|generic (SeaweedFS and MinIO are tested in CI; see object-storage.md)

# In another terminal: install scn and check the connection
cargo install --path client/cli
scn doctor
scn config set user.name "Ada Lovelace"       # default commit author (falls back to git's user.name / user.email)
scn config set user.email ada@example.com
scn repo create mono                # repositories and branches: scn repo / scn branch (see repos.md)

# Everyday workflow
scn clone mono && cd mono          # check out (ref main by default; empty repositories work too)
echo 'hello' > README.md
echo 'target/' > .gitignore         # .gitignore and .scionignore are honored (gitignore syntax)
scn status                          # A .gitignore, A README.md
scn commit -m "add readme"          # or --author "Ada <ada@example.com>"
scn restore src/main.rs             # discard local changes to tracked files (`.` for all); added files are kept
scn commit --amend                  # fold more local changes into the pending commit (keeps message and author)
scn uncommit                        # drop the pending commit, keep its changes as local changes
scn land                            # land on main; if main moved, the server rebases (three-way merge)
scn sync                            # pull others' changes; rebases a pending commit at file level
scn tag v1.0                        # tag the current commit; `scn tag` lists, `scn log v1.0` uses it
scn switch -c feature               # create a branch at the current snapshot and switch to it; `scn switch main` back
scn merge feature                   # merge a branch as a pending merge commit (three-way, line level); then `scn land`
# On conflicts (scn sync or scn merge) files get <<<<<<< / >>>>>>> markers and `scn status` shows C:
scn resolve src/main.rs             # after editing out the markers; or --ours / --theirs to take one side
scn commit                          # finishes it, reusing the original message; `scn merge --abort` undoes a merge
scn cat main:README.md              # read files without a working copy (add --repo outside one)
scn ls main:                        # list a directory; every command supports --json
scn log -n 10 -- src                # history, newest first, optionally limited to a path (git-style: HEAD~1, A..B)
scn diff                            # uncommitted local changes (unified diff)
scn diff main                       # what the tip of main changed; `scn diff A B` compares two revisions
scn blame src/lib.rs                # the commit that last changed each line
scn clone mono part --sparse src/   # sparse checkout: only src/ and files at the root; `scn sparse add docs/` later
scn archive -o mono.tar.gz          # download a snapshot as tar.gz / zip / tar (see archive.md)

# scn talks to the server through a local daemon (scn daemon), started on demand; SCION_NO_DAEMON=1 opts out
scn daemon status

# A regional cache in front of one or more servers; advertise it with scion server --read-endpoint
# (see cache.md)
cargo run -p scion -- cache --upstream http://127.0.0.1:7080 --listen 127.0.0.1:7090 --warm mono:main

# Import an existing git repository with its history, authors and timestamps
# (see git-import.md)
scn import git ~/src/my-repo --repo mono

# After upgrading a server that already has data, before starting it: repair lineage and backfill derived indexes (idempotent)
cargo run -p scion -- admin reindex --lite --data-dir ./.scion-data
```

## What a checkout will not write

A repository's paths come from the server, so a hostile repository must not be able to change anything outside the working copy. `scn` refuses to write or remove, and stops with `refusing to write <path>`:

- paths with a `.scion` or `.git` component, in any case (`.SCION`, `.Git`): the working copy's own metadata, and git's;
- anything but plain relative paths (no `..`, empty components or backslashes);
- paths that lead through a symbolic link, such as a link left by an earlier version where the new version has a directory, or a link `A` next to `a/...` on a case-insensitive file system (macOS, Windows).

Symbolic links themselves are checked out as links, as git does; only writes through them are refused.
