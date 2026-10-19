# Scion on a real repository: starhub-server

*2026-10-19 · Scion `main` at the time of writing*

Two rounds of testing on the history of [starhub-server](https://github.com/OpenCSGs/csghub-server):

1. **[Full real-world test](#1-full-real-world-test).** The repository is imported into Scion. Every feature is then exercised, from reading and history to large changes, collaboration, conflicts, branches, sparse checkout and large files, and the results are checked against git.
2. **[Side-by-side comparison](#2-side-by-side-with-starhubs-git-stack-gitaly--minio).** Scion is compared with the git stack starhub-server runs today, Gitaly with MinIO. The same workload runs on both, and the results are compared one request at a time.

The scripts are in [`bench/starhub/`](../../bench/starhub/README.md).

## Summary

- **Correctness.** Scion's results match git wherever the two define the same thing:
  - files at any revision, directory listings and executable bits;
  - git SHAs, diffs, single commits and archives (tar.gz and zip);
  - blame along the first-parent chain;
  - commit counts.

  The first round found 4 bugs, all now fixed on `main`.
- **Speed.** Against Gitaly as starhub drives it:
  - reads are 10–150× faster, and concurrent read throughput is 5–8× higher;
  - writes are 5–15× faster;
  - 16 writers committing to one branch at the same time all succeed. With starhub's `CommitFiles`, 94% of the same commits fail.
- **The main gap is how history is attributed.** starhub merges almost everything through merge requests: 2,776 of the 2,848 commits on main's first-parent chain are merges.
  - Scion attributes changes along the first-parent chain, so the last commit of each file, blame and per-file history mostly name the merge commit.
  - Gitaly follows git and names the original commit on the feature branch.

  This has to change before starhub can use Scion.
- **Storage and import cost more than git.**
  - Objects take about 3.5× the bytes of git's delta-compressed pack.
  - A first import takes 154 s, where Gitaly takes 3 s to receive a bundle.

---

## 1. Full real-world test

### Setup

- **Repository:** starhub-server, main with 6,312 commits (2,836 merges) and 4,663 files, plus two branches and one tag.
- **Server:** a Lite-tier server (SQLite and a local object directory, zstd compression on), on one machine.
- **Script:** [`bench/starhub/fulltest.py`](../../bench/starhub/fulltest.py). It starts a fresh server and runs 75 checks. Wherever possible, a check compares Scion's answer with git's.

### Results: 74 of 75 checks pass

| Area | Checks | Result |
|---|---|---|
| Import | main, two branches and a tag | ✅ |
| Checkout | full clone: all 4,663 files, executable bits and symlinks equal `git archive`; sparse clone of two directories | ✅ |
| Reading | `cat` of 40 random files at 40 random revisions, byte for byte against `git show`; `ls` at old revisions; archive as tar.gz and zip | ✅ |
| History | full log (6,312 commits, each once); `log -- PATH` for 15 paths against git's first-parent changes; 25 random `diff A..B` and `A...B` against `git diff --name-status`; `show` of 20 commits including merges; 30 git SHAs; `~N`, `^`, `^2`, `HEAD`, `@` | ✅ |
| Blame | 15 files, every line's author and time against `git blame --first-parent` | ⚠️ 14 files equal; one file has 2 lines that differ (see [open issues](#open-issues-from-round-1)) |
| Large change | 1,500 files modified, 600 added, 300 deleted, 100 moved, mode changes and symlinks in one commit; status, diff, commit, land; the server snapshot and a second working copy both equal the first | ✅ |
| Ignore files | starhub's own `.gitignore` (`bin`, `.env`, `local_only`, `.DS_Store`) | ✅ |
| Two developers | disjoint changes, so the server rebases at land; different lines of the same 30 files, merged line by line during sync | ✅ |
| Conflicts | 25 files with the same line changed: land refuses, sync writes markers (exit 3), commit refuses, resolve refuses leftover markers; then resolved with `--theirs`, `--ours` and by hand, and committed with the original message | ✅ |
| Branches | switch to a new branch (205 files rewritten), 5 conflicting files, `merge --abort`, merge again and resolve; result is a 2-parent merge commit; merging again is a no-op; merging an imported git branch | ✅ |
| Undo | restore of a directory and of everything, amend, uncommit, tags, transitional git spellings | ✅ |
| Sparse checkout | sync after many upstream commits; `sparse add`, commit, `sparse rm`, `sparse disable` | ✅ |
| Large files | a 40 MB random file and a 32 MB text file: commit, read back, sync; 30 small commit + land cycles | ✅ |
| Server | `fsck --verify` (6,363 commits, 33,650 trees, 31,051 files: no problems); restart; no errors in the logs | ✅ |

### Performance (Lite tier, one machine)

| Operation | Time |
|---|---|
| Import main (6,312 commits) | 59.5 s (106 commits/s) |
| Import a branch or tag sharing that history | 0.5 s / 27 ms (after fix 2 below) |
| Full clone (4,663 files) / sparse clone | 384 ms / 87 ms |
| `cat` at any revision | median 9 ms |
| `log -n 50` / full log (6,312 entries) | 10 ms / 45 ms |
| `log -- PATH` | median 16 ms, max 74 ms |
| `diff A..B` / `show` / `blame` | 12 ms / 9 ms / 14 ms |
| 3,000+ changes: `status` / `commit` / `land` / `sync` elsewhere | 64 ms / 1.9 s / 46 ms / 311 ms |
| Land with server rebase / sync with line merges in 30 files | 84 ms / 116 ms |
| Commit + land of 72 MB of large files / read back 40 MB | 412 ms / 337 ms |
| Small change, commit + land | 89 ms per cycle |
| Server restart until ready | 46 ms |
| `fsck --verify` of the repository | 6.2 s |

### Fixed after round 1

| # | Problem | Fix |
|---|---|---|
| 1 | Importing a git tag into `tags/…` failed: the importer moved the ref commit by commit, and tags never move | The tag is created at the tagged commit in one step (`8cf20a1`) |
| 2 | `scn log` / `show` showed the author's name with the committer's date | They show the author date, as git does (`8cf20a1`) |
| 3 | Diffs were sometimes longer than `git diff --minimal` (4 of 300 files) | Exact LCS diffs when the changed region fits a ~32 MB table, else Myers: 299 of 300 match (`737a465`) |
| 4 | Re-importing, or importing a branch that shares history, read all 6,000+ commits from git again (39–40 s) | The importer skips commits the server already has: 0.5 s per branch, 27 ms per tag (`e29f760`) |
| — | Objects were stored uncompressed (1,007 MB of object bytes) | zstd compression: 227 MB (`0120be9`) |

### Open issues from round 1

- **Two lines of one blame differ from git.** Both sides now produce minimal diffs. git additionally slides ambiguous hunks (identical lines such as `}`) by indentation, and we do not port that heuristic.
- **Blame and path history do not follow renames or copies.** This is a known trade-off and is in the [roadmap](../design/roadmap.md).
- **Small objects cost disk space on a local file system.** 84% of the objects are under 4 KB, so a Lite data directory takes 404 MB on disk with 4 KB blocks, against 227 MB of bytes. Packing small objects is in the roadmap.

---

## 2. Side by side with starhub's git stack (Gitaly + MinIO)

### Setup

| | starhub today (baseline) | Scion |
|---|---|---|
| Components | Gitaly v16.4.6 and MinIO, as in starhub's `docker-compose-gitaly.yml` | scion server, PostgreSQL 16 and the same MinIO instance (Standard tier) |
| Driven by | **starhub's own Gitaly client** (`builder/git/gitserver/gitaly`), plus raw Gitaly RPCs where that client has no wrapper (blame, path history, bundle import) | Scion's Rust SDK and `scn` |
| git protocol | `git clone` / `git push` through starhub's smart HTTP code (`InfoRefsResponse`, `UploadPack`, `ReceivePack`); hooks call a stub of starhub's internal API with the same responses | — |
| Data | starhub-server main (6,326 commits), two branches, one tag | same |

Both servers run in Docker on the same machine (arm64 images, no emulation), and the test programs run on the host.

Every read in the workload uses git SHAs, so both sides answer exactly the same requests. The workload is 255 file reads, 40 directories, 40 history pages, 30 path histories, 40 commits, 25 diffs, 30 blames and 3 archives, followed by the writes.

### Correctness

| Operation | Compared | Equal | Notes |
|---|---|---|---|
| Read file (old revisions, large files) | 252 | ✅ 252 | Byte for byte |
| List directory | 38 | ✅ 38 | |
| Single commit | 40 | ✅ 40 | |
| Diff between two commits (file list) | 25 | ✅ 25 | |
| Archive zip at 3 revisions | 4,675 / 3,541 / 1,992 files | ✅ all | Same files, same bytes |
| Branch list | 3 | ✅ | |
| History, 50 entries | 40 | ⚠️ 49 of 50 entries equal | Order of commits at the page boundary |
| **Last commit per entry** | 38 directories | ❌ 53 of 56 entries differ | Merge attribution, see below |
| **Path history** | 30 | ❌ about half in common | Merge attribution |
| **Blame** | 30 files | ❌ different commits | Merge attribution |

**Merge attribution.** Scion attributes a change along the first-parent chain, so a change that arrived with a merge request belongs to the merge commit. Gitaly follows git: it walks into the merged branch and names the commit that made the change. In three cases checked by hand:

- Gitaly's commit is on the feature branch and not on main's first-parent chain.
- Scion's commit is the 2-parent merge that contains it, for example *Merge branch 'feature-runner-operator' into 'main'*.

With starhub's merge-request workflow, the last-commit column of every file list, blame and per-file history in starhub's UI would mostly show "Merge branch …".

### Read latency (ms, p50 / p95 / p99)

| Operation | Gitaly | Scion |
|---|---|---|
| Read file | 38 / 42 / 56 | **1.1 / 3.2 / 11** |
| List directory (starhub's `GetRepoFileTree`) | 138 / 1,650 / 1,665 | **0.9 / 1.3 / 1.4** |
| Last commit per entry (`GetLogsTree`) | 94 / 1,603 / 1,619 | **2.4 / 27 / 43** |
| History, 50 entries | 63 / 70 / 76 | **12 / 21 / 21** |
| Path history, 50 entries | 49 / 71 / 72 | **4.7 / 10 / 12** |
| Single commit with its changes | 74 / 81 / 102 | **6.8 / 16 / 21** |
| Diff between two commits | 68 / 76 / 79 | **14** / 54 / 84 |
| Blame | 41 / 59 / 62 | **4.4 / 15 / 27** |
| Archive zip (~40 MB) | 4,041 / 4,594 | **2,083 / 2,587** |
| Branch list | 40 | **0.8** |
| Repository size | 186 | 266 (different meaning*) |

\* starhub's `GetRepoSize` reports 935 MB. Scion's `GetTreeStats` reports 102 MB, the total size of the files on main. They measure different things.

### Concurrent reads (random file reads)

| Concurrency | Gitaly | Scion |
|---|---|---|
| 1 | 25 req/s (p99 53 ms) | **363 req/s** (p99 2.7 ms) |
| 8 | 72 req/s (p99 160 ms) | **382 req/s** (p99 8.4 ms) |
| 32 | 73 req/s (p99 458 ms) | **552 req/s** (p99 13 ms) |
| 64 | 71 req/s (p99 902 ms) | **613 req/s** (p99 17 ms) |

Gitaly levels off at about 72 req/s, with CPU peaking at 710% while git subprocesses run. Scion's CPU peaked at 95%.

### Writes

| Operation | Gitaly (starhub's `CommitFiles`) | Scion (commit + land) |
|---|---|---|
| Edit one file, as the web editor does | 285 ms (p99 355) | **21 ms** (p99 41) |
| One commit of 200 files | 4,293 ms | **286 ms** |
| One 20 MB file | 931 ms | **185 ms** |
| **16 writers, one branch, 160 commits** | ❌ **150 failed** ("reference does not point to expected object"); 10 commits on main | ✅ **all 160 on main** (server rebase), 0.59 s wall time |
| Create / delete a branch | 291 / 72 ms | **0.9 / 1.0 ms** |
| `git push` vs `scn commit` + `land`, 1 file | 880 ms | **129 ms** |
| Same, 50 files | 912 ms | **372 ms** |
| Same, 500 files | ~0.9 s† | 1,159 ms |

† Median of two repeat runs. The first 500-file push took 37 s, probably Gitaly housekeeping after the write workload left 1,300 loose objects.

starhub's `CommitFiles` sends no expected parent and does not retry, so concurrent edits to one branch mostly fail.

### Checkout and working copy

| Scenario | git through Gitaly | Scion |
|---|---|---|
| Full checkout | 2.0 s (full history) | **1.1 s** (snapshot) |
| `--depth 1` / `--filter=blob:none` | 1.15 s / 1.63 s | — |
| Sparse checkout of two directories (1,511 files) | 0.79 s (blob:none + sparse-checkout) | **0.11 s** |
| Metadata in the working copy | `.git` 82.8 MB (shallow 38.7, partial 45.6) | **`.scion` 1.1 MB** |
| 10 concurrent checkouts | 4.8 s (shallow) | 5.0 s |
| `status` on a clean tree | **17 ms** | 22 ms |

### Storage and import

| | Gitaly | Scion |
|---|---|---|
| Same four refs | **82 MB** (one delta-compressed pack) | 230 MB of objects in MinIO + 50 MB of metadata in PostgreSQL; **546 MB on MinIO's disk** |
| After the write workload | 86.6 MB | 235 MB + 53 MB |
| First import | **3.0 s** (receives the bundle's pack) | **154 s** (rewrites every commit; PostgreSQL and MinIO over Docker's network) |
| Further branch / tag | — | 1.1–1.3 s / 47 ms |

git keeps successive versions of a file as deltas in one pack. Scion compresses each object on its own. MinIO also keeps per-object metadata, which roughly doubles the disk use for tens of thousands of small objects.

### Deployment and resources

| | Gitaly + MinIO | Scion + PostgreSQL + MinIO |
|---|---|---|
| Server image | 353 MB | **50 MB** (+ PostgreSQL 159 MB) |
| Restart until serving | 5.1 s | **0.15 s** |
| Memory, idle | Gitaly 184 MiB | Scion **8 MiB** + PostgreSQL 78 MiB |
| Memory, peak under the workload | Gitaly 381 MiB | Scion 637 MiB + PostgreSQL 172 MiB |
| CPU, peak | 710% | 95% (Scion) + 39% (PostgreSQL) |

### Conclusions

**Scion is ahead on:**

- read latency and concurrent reads;
- write latency, and concurrent writes to one branch that neither fail nor lose changes;
- sparse checkout and the size of the working copy;
- deployment size and startup time.

On contents, listings, diffs and archives it matches Gitaly exactly.

**To fix before starhub adopts it, in order:**

1. **Git-compatible history attribution.** Last commit per entry, blame and path history need to follow merges into the merged branch, as git does. Without it starhub's UI shows "Merge branch …" almost everywhere.
2. **Storage.** Options are deltas between versions of a file, and packing small objects, which also removes MinIO's per-object overhead. Today Scion uses about 3.5× git's bytes, and 6.6× on MinIO's disk.
3. **Import speed for migrations.** A server-side import, roadmap item 2.6, would replace the client-side commit-by-commit import.
4. **Memory under load.** Memory grows from 8 to 637 MiB; check which caches account for it and bound them.

### Limits of the comparison

- One machine and one run; some operations (archive, repository size, the large file) have small samples.
- The baseline goes through starhub's client code, so it includes starhub's own overhead. For example, `GetRepoFileTree` gathers extra details, which makes some listings slow. That is how starhub behaves, but it is not Gitaly's raw speed.
- Repository size means different things on the two sides.
- Gitaly's import receives a ready-made pack, while Scion's rewrites every commit. The import times compare migration paths, not storage engines.

## Reproducing

See [`bench/starhub/README.md`](../../bench/starhub/README.md). Both scripts start from empty data:

- `fulltest.py` runs round 1 against a Lite server.
- `compare.py` builds the Docker setup, imports into both systems, and runs round 2.
