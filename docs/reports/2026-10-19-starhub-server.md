# Scion on a real repository: starhub-server

*2026-10-19 · Scion `main` at the time of writing*

Two rounds of testing on the history of [starhub-server](https://github.com/OpenCSGs/csghub-server):

1. **[Full real-world test](#1-full-real-world-test).** The repository is imported into Scion. Every feature is then exercised, from reading and history to large changes, collaboration, conflicts, branches, sparse checkout and large files, and the results are checked against git.
2. **[Side-by-side comparison](#2-side-by-side-with-starhubs-git-stack-gitaly--minio-and-with-git-itself).** Scion is compared with the git stack starhub-server runs today (Gitaly with MinIO), and with git itself through libgit2. The same workload runs on all three, and the results are compared one request at a time.

The scripts are in [`bench/starhub/`](../../bench/starhub/README.md).

## Summary

- **Correctness.** Scion's results match git wherever the two define the same thing:
  - files at any revision, directory listings and executable bits;
  - git SHAs, diffs, single commits and archives (tar.gz and zip);
  - blame along the first-parent chain;
  - commit counts.

  The first round found 4 bugs, all now fixed on `main`.
- **Speed.**
  - Against Gitaly as starhub drives it, Scion's reads are 4–40× faster and its writes 5–170× faster. Most of that gap is Gitaly's own overhead, not git's.
  - Against git itself (libgit2 in process, no network), git's reads are 6–100× faster, mostly because Scion is a server; Scion's history and diff pages are slower than that alone explains.
  - 16 writers committing to one branch at the same time all succeed on Scion. On git, 84–94% of the same commits fail unless each writer retries.
- **The main gap is how history is attributed.** starhub merges almost everything through merge requests: 2,776 of the 2,848 commits on main's first-parent chain are merges.
  - Scion attributes changes along the first-parent chain, so the last commit of each file, blame and per-file history mostly name the merge commit.
  - Gitaly follows git and names the original commit on the feature branch.

  This has to change before starhub can use Scion.
- **Storage and import cost more than git.**
  - With packs (added after the first comparison), objects take 282 MB of MinIO disk instead of 546 MB, against git's 82 MB pack.
  - A first import takes 99 s (154 s before packs), where Gitaly takes 5 s to receive a bundle.

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

## 2. Side by side with starhub's git stack (Gitaly + MinIO), and with git itself

### Setup

Three systems answer the same workload:

| | Gitaly (starhub today) | libgit2 (git's lower bound) | Scion |
|---|---|---|---|
| What | Gitaly v16.4.6 and MinIO, as in starhub's `docker-compose-gitaly.yml` | The bare repository itself, through libgit2 ([git2-rs](https://github.com/rust-lang/git2-rs)) in the benchmark process. Where libgit2 has no API (last commit per entry, path history, archives), the git command line, as a git server would run it | scion server, PostgreSQL 16 and the same MinIO (Standard tier) |
| Driven by | **starhub's own Gitaly client** (`builder/git/gitserver/gitaly`), plus raw Gitaly RPCs for blame, path history, and the "raw" rows below | `gitbench`, in the same Docker VM as the servers | Scion's Rust SDK and `scn` |
| Between caller and data | gRPC, Gitaly, a git process per request, starhub's hooks on writes | **nothing**: no network, no server, no hooks | gRPC, scion server, PostgreSQL and MinIO |
| Data | starhub-server main (6,326 commits), two branches, one tag | the same bundle, cloned bare | same |

Everything runs in Docker on one machine (arm64, no emulation). Every request names git SHAs, so all three answer exactly the same questions. Each server is restarted right before its workload, so none starts with objects in memory.

The workload is 255 file reads, 40 directories, 40 history pages, 30 path histories, 40 commits, 25 diffs, 30 blames and 3 archives, followed by the writes.

#### Is the comparison fair?

An earlier version of this section compared Scion with Gitaly only, and some of its rows favoured Scion. They are corrected here:

| Issue | Correction |
|---|---|
| starhub's directory listing (`GetRepoFileTree`) also computes each entry's last commit (`ListLastCommitsForTree`) and reads every file in it (`GetBlobs`). Scion's listing does neither | A "raw RPC" row with `GetTreeEntries`. The starhub row stays, since it is what starhub does |
| starhub's single commit (`GetSingleCommit`) also fetches the whole patch (`DiffStats` + `RawDiff`). Scion's returns the changed files only | A raw row with `FindCommit` + `FindChangedPaths` |
| Every Gitaly request crosses Gitaly's gRPC layer and starts git processes; writes also run starhub's hooks. That says how Gitaly performs, not how fast git is | The libgit2 column: the same operations on the same repository with nothing in between |
| The checkouts before the workload read all of main into Scion's in-memory cache | Each server restarts before its workload |

The libgit2 column has the opposite bias: it pays no network or server costs, which Scion always does. A Scion request that does nothing (listing branches) takes about 1 ms, which is the floor of its column.

### Correctness

| Operation | libgit2 = Gitaly | Scion = Gitaly | Notes |
|---|---|---|---|
| Read file (old revisions, large files) | ✅ 252 / 252 | ✅ 252 / 252 | byte for byte |
| List directory | ✅ 38 / 38 | ✅ 38 / 38 | also equal to Gitaly's raw `GetTreeEntries` |
| Single commit | ✅ 40 / 40 | ✅ 40 / 40 | also equal to the raw RPCs |
| Diff between two commits (file list) | ✅ 25 / 25 | ✅ 25 / 25 | |
| Archive zip at 3 revisions | ✅ | ✅ | same files, same bytes |
| Path history, 50 entries | ✅ 30 / 30 | ❌ 0 / 30 | merge attribution, see below |
| Last commit per entry | 35 / 38 | ❌ 0 / 38 | merge attribution |
| Blame | 29 / 30 | ❌ 0 / 30 | merge attribution |
| History, 50 entries | 15 / 40 | 0 / 40 (49 of 50 entries in common) | ordering of commits with equal dates |

libgit2 and Gitaly agree almost everywhere (both are git; the few differences are history simplification corner cases and ordering), which confirms the harness. Scion differs from both in the same places.

**Merge attribution.** Scion attributes a change along the first-parent chain, so a change that arrived with a merge request belongs to the merge commit. git walks into the merged branch and names the commit that made the change. In three cases checked by hand:

- git's commit is on the feature branch and not on main's first-parent chain.
- Scion's commit is the 2-parent merge that contains it, for example *Merge branch 'feature-runner-operator' into 'main'*.

With starhub's merge-request workflow, the last-commit column of every file list, blame and per-file history in starhub's UI would mostly show "Merge branch …".

### Read latency (ms, p50 / p95 / p99)

| Operation | libgit2 (no network) | Gitaly | Scion |
|---|---|---|---|
| Read file | **0.04 / 0.4 / 1.3** | 41 / 51 / 63 | 2.3 / 7.3 / 15 |
| List directory | **0.01 / 0.02 / 0.03** | 52 / 58 / 59 (raw RPC); 146 / 1,718 / 1,887 (starhub) | 1.4 / 2.8 / 4.5 |
| Last commit per entry | 7.7 / 168 / 211 (`git log`) | 104 / 1,766 / 2,242 | **2.7 / 50 / 70** † |
| History, 50 entries | **1.9 / 3.1 / 24** | 67 / 82 / 86 | 18 / 32 / 41 |
| Path history, 50 entries | 17 / 79 / 91 (`git log`) | 52 / 76 / 81 | **6.7 / 14 / 19** † |
| Single commit with changed files | **0.7 / 1.4 / 1.7** | 70 / 74 / 76 (raw RPC); 78 / 90 / 104 (starhub, with patch) | 4.5 / 14 / 14 |
| Diff between two commits | **0.8 / 1.7 / 1.9** | 74 / 83 / 88 | 10 / 46 / 89 |
| Blame | 40 / 482 / 502 (libgit2); 9.8 / 25 / 26 (`git blame`) | 41 / 64 / 65 | **4.8 / 19 / 32** † |
| Archive zip (~40 MB) | 2,336 / 2,588 (`git archive`) | 4,374 / 4,638 | **2,177** / 3,316 |
| Branch list | **0.1** | 41 | 1.2 |

† Not the same work: Scion follows the first-parent chain only, which is cheaper than git's walk into merged branches. Expect these to slow down once Scion attributes changes the way git does.

### Concurrent reads (random file reads, ms per request, p50 / p99)

| Readers | libgit2 (a repository handle per thread) | Gitaly | Scion |
|---|---|---|---|
| 1 | **0.04 / 1.3** | 41 / 82 | 0.9 / 2.5 |
| 8 | **0.1 / 1.3** | 113 / 166 | 2.5 / 11 |
| 32 | **0.4 / 2.7** | 489 / 611 | 6.1 / 12 |
| 64 | **0.6 / 5.1** | 972 / 1,108 | 10 / 17 |

Gitaly levels off at about 60 requests per second, with its CPU peaking at 730% while git processes run. Scion serves 370–570 requests per second with its CPU peaking at 76%.

### Writes

| Operation | libgit2 (no network, no hooks) | Gitaly (starhub's `CommitFiles`, with hooks) | Scion (commit + land) |
|---|---|---|---|
| Edit one file | **0.9** / 2.0 ms | 330 / 392 ms | 18 / 24 ms |
| One commit of 200 files | 67 ms | 4,783 ms | **28 ms** |
| One 20 MB file | **57 ms** | 969 ms | 200 ms |
| 16 writers, one branch, 160 commits, no retry | ❌ 135 failed (ref locked or moved) | ❌ 150 failed ("reference does not point to expected object") | ✅ **all 160** (the server rebases), 0.41 s |
| Same, each writer retrying until it succeeds | ✅ all 160, 0.17 s | — | — |
| Create / delete a branch | **0.05 / 0.05 ms** | 304 / 77 ms | 1.0 / 0.9 ms |
| `git push` vs `scn commit` + `land`, 1 / 50 / 500 files | — | 12,182‡ / 928 / 1,314 ms | **205 / 258 / 383 ms** |

‡ An outlier, as in the earlier run: probably Gitaly housekeeping after the write workload left 1,300 loose objects. The previous run measured 880–950 ms.

git's ref update is a compare-and-swap on one file, so concurrent writers to one branch fail unless each retries, and a retry has to rebuild its commit on the new tip. Scion's land does that rebase on the server for every writer.

### Checkout and working copy

| Scenario | git through Gitaly | Scion |
|---|---|---|
| Full checkout | 2.9 s (full history) | **2.5 s** (snapshot) |
| `--depth 1` / `--filter=blob:none` | 1.6 s / 2.3 s | — |
| Sparse checkout of two directories (1,511 files) | 1.1 s (blob:none + sparse-checkout) | **0.16 s** |
| Metadata in the working copy | `.git` 82.8 MB (shallow 38.7, partial 45.6) | **`.scion` 1.1 MB** |
| 10 concurrent checkouts | 10.7 s (shallow) | **6.1 s** |
| `status` on a clean tree | **21 ms** | 46 ms |

This run is slower across the board than the previous one (full clone 2.0 s and 1.1 s), because a build of the whole workspace ran on the same machine at the same time. Compare the columns with each other, not with other runs.

### Storage and import

| | Gitaly | Scion, loose objects | Scion, packs (now) |
|---|---|---|---|
| Objects in MinIO | — | 67,636 | **10,065** |
| Object bytes | **82 MB** (one delta-compressed pack) | 230 MB | 233 MB |
| MinIO disk | — | 546 MB | **282 MB** |
| Metadata in PostgreSQL | — | 50 MB | 63 MB (+13 MB of pack locations) |
| First import | **5.4 s** (receives the bundle's pack) | 154 s | **99 s** |
| Further branch / tag | — | 1.1–1.3 s / 47 ms | 1.6–2.0 s / 43 ms |

Since this comparison, Scion stores objects written together in packs (see [Object storage](../guide/object-storage.md#packs)). That removes most of MinIO's per-object cost and a third of the import time. Object bytes are still about 3× git's, because git stores successive versions of a file as deltas; that is the next step.

### Deployment and resources

| | Gitaly + MinIO | Scion + PostgreSQL + MinIO |
|---|---|---|
| Server image | 353 MB | **50 MB** (+ PostgreSQL 159 MB) |
| Restart until serving | 5.6 s | **0.14 s** |
| Memory, idle | Gitaly 195 MiB | Scion **8 MiB** + PostgreSQL 92 MiB |
| Memory, peak under the workload | Gitaly 420 MiB | Scion 737 MiB + PostgreSQL 181 MiB |
| CPU, peak | 732% | 76% (Scion) + 38% (PostgreSQL) |

### Conclusions

**Against git itself** (libgit2, no network):

- git reads are 6–100× faster than Scion's. Most of the gap is the round trip a server always costs (about 1 ms here), but not all: Scion's history page (18 ms) and diff (10 ms) are much slower than libgit2's (2 ms and 1 ms) plus a round trip. Those two are worth optimising.
- A single small commit is faster in git (1 ms against 18 ms); a commit of 200 files is faster in Scion (28 ms against 67 ms).
- Concurrent commits to one branch fail in git unless the writer retries; Scion's land rebases them on the server.

**Against Gitaly as starhub runs it**, Scion is far ahead on every read and write. Most of that gap is Gitaly's own overhead (a git process per request, its gRPC layer, starhub's hooks and extra work in starhub's client), not git's storage: Gitaly's raw directory listing takes 52 ms where libgit2 takes 0.01 ms.

Scion is also ahead on sparse checkout, working copy size, and deployment size and startup. On contents, listings, diffs and archives it matches git exactly.

**To fix before starhub adopts it, in order:**

1. **Git-compatible history attribution.** Last commit per entry, blame and path history need to follow merges into the merged branch, as git does.
2. **Storage.** Packs (done) removed MinIO's per-object overhead. Next: deltas between versions of a file inside packs, then repacking existing objects, then smaller metadata.
3. **History and diff latency**, against libgit2's numbers.
4. **Import speed for migrations.** A server-side import, roadmap item 2.6.
5. **Memory under load.** Memory grows from 8 to 737 MiB; check which caches account for it and bound them.

### Limits of the comparison

- One machine and one run; some operations (archive, repository size, the large file) have small samples. A workspace build ran during this run, which slowed the checkouts.
- The libgit2 column pays no network or server cost. A git server built on libgit2 would add at least a round trip to each of its numbers.
- Where libgit2 has no API, its column measures the git command line, including starting a process.
- Repository size means different things on the two sides (starhub's `GetRepoSize` reports 935 MB, Scion's `GetTreeStats` the 102 MB of files on main), so it is not compared.
- Gitaly's import receives a ready-made pack, while Scion's rewrites every commit. The import times compare migration paths, not storage engines.

## Reproducing

See [`bench/starhub/README.md`](../../bench/starhub/README.md). Both scripts start from empty data:

- `fulltest.py` runs round 1 against a Lite server.
- `compare.py` builds the Docker setup, imports into both systems, and runs round 2.
