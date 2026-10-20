# Scion on a real repository: starhub-server

*2026-10-19, round 2 rerun 2026-10-20 · Scion `main` at the time of writing*

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
  - Against Gitaly as starhub drives it, Scion's reads are 10–50× faster (archives 2×) and its writes 4–300× faster. Most of that gap is Gitaly's own overhead, not git's.
  - Against git itself (libgit2 in process, no network), Scion's reads take 1–3 ms more, mostly the round trip any server costs; its history pages are faster than libgit2's even so.
  - 16 writers committing to one branch at the same time all succeed on Scion. On git, about 90% of the same commits fail unless each writer retries.
- **The main gap is how history is attributed.** starhub merges almost everything through merge requests: 2,776 of the 2,848 commits on main's first-parent chain are merges.
  - Scion attributes changes along the first-parent chain, so the last commit of each file, blame and per-file history mostly name the merge commit.
  - Gitaly follows git and names the original commit on the feature branch.

  This has to change before starhub can use Scion.
- **Storage is close to git's after a repack; import is still slow.**
  - Stored objects went from 546 MB of MinIO disk (one stored object each) to 283 MB (objects written together share a pack) to 86 MB after `scion admin repack` (deltas between versions), against Gitaly's 82 MB repository.
  - The metadata in PostgreSQL takes another 54 MB, which git keeps inside its pack.
  - A first import takes 94 s plus a 24 s repack, where Gitaly takes 3 s to receive a bundle.

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
- **Small objects cost disk space on a local file system.** 84% of the objects are under 4 KB, so a Lite data directory took 404 MB on disk with 4 KB blocks, against 227 MB of bytes. Fixed since: objects are stored in packs, and `scion admin repack` stores versions as deltas (see [round 2](#storage-and-import)).

---

## 2. Side by side with starhub's git stack (Gitaly + MinIO), and with git itself

### Setup

Three systems answer the same workload:

| | Gitaly (starhub today) | libgit2 (git's lower bound) | Scion |
|---|---|---|---|
| What | Gitaly v16.4.6 and MinIO, as in starhub's `docker-compose-gitaly.yml` | The bare repository itself, through libgit2 ([git2-rs](https://github.com/rust-lang/git2-rs)) in the benchmark process. Where libgit2 has no API (last commit per entry, path history, archives), the git command line, as a git server would run it | scion server, PostgreSQL 16 and the same MinIO (Standard tier) |
| Driven by | **starhub's own Gitaly client** (`builder/git/gitserver/gitaly`), plus raw Gitaly RPCs for blame, path history, and the "raw" rows below | `gitbench`, in the same Docker VM as the servers | Scion's Rust SDK and `scn` |
| Between caller and data | gRPC, Gitaly, a git process per request, starhub's hooks on writes | **nothing**: no network, no server, no hooks | gRPC, scion server, PostgreSQL and MinIO |
| Data | starhub-server main (6,338 commits), two branches, one tag | the same bundle, cloned bare | same, imported, then `scion admin repack` |

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
| Read file | **0.05 / 0.5 / 1.4** | 40 / 50 / 72 | 1.2 / 3.8 / 24 |
| List directory | **0.01 / 0.04 / 0.05** | 52 / 62 / 69 (raw RPC); 140 / 1,717 / 1,824 (starhub) | 1.0 / 1.8 / 2.2 |
| Last commit per entry | 8.8 / 203 / 246 (`git log`) | 98 / 1,617 / 1,708 | **2.3 / 28 / 71** † |
| History, 50 entries | 1.9 / 2.8 / 25 | 68 / 76 / 94 | **1.6 / 2.0 / 3.7** |
| Path history, 50 entries | 19 / 41 / 44 (`git log`) | 52 / 76 / 84 | **4.3 / 6.7 / 8.5** † |
| Single commit with changed files | **0.9 / 2.4 / 2.6** | 70 / 82 / 93 (raw RPC); 78 / 101 / 112 (starhub, with patch) | 3.1 / 6.8 / 6.8 |
| Diff between two commits | **0.8 / 1.7 / 2.6** | 72 / 133 / 149 | 4.0 / 14 / 24 |
| Blame | 42 / 507 / 516 (libgit2); 10 / 27 / 29 (`git blame`) | 45 / 70 / 74 | **4.1 / 16 / 27** † |
| Archive zip (~40 MB) | 2,357 / 2,574 (`git archive`) | 4,487 / 4,902 | **2,070 / 2,390** |
| Branch list | **0.3** | 42 | 1.0 |

† Not the same work: Scion follows the first-parent chain only, which is cheaper than git's walk into merged branches. Expect these to slow down once Scion attributes changes the way git does.

These Scion numbers are after `scion admin repack` (see [Storage](#storage-and-import)): every object is read from one 27 MB pack, through a cache of 1 MiB blocks, and old versions through delta chains of up to 50.

This run was 5–20% slower than the previous one on most rows, writes included, which the repack changes do not touch. To check that the longer delta chains cost nothing, the same server and data were repacked alternately with chains of at most 16 and 50, and the read workload ran after each repack, from a cold restart, twice per setting. Every row was within run-to-run noise between the two (for example, diffs 3.3–3.4 ms, blame 1.5–1.6 ms, file reads 1.1 ms in all four runs), while the pack was 32.5 MB with chains of 16 and 27.8 MB with chains of 50.

### Concurrent reads (random file reads, ms per request, p50 / p99)

| Readers | libgit2 (a repository handle per thread) | Gitaly | Scion |
|---|---|---|---|
| 1 | **0.04 / 1.3** | 41 / 87 | 1.1 / 2.7 |
| 8 | **0.06 / 1.3** | 107 / 177 | 2.7 / 7.4 |
| 32 | **0.3 / 2.7** | 465 / 554 | 5.6 / 17 |
| 64 | **0.5 / 4.5** | 945 / 1,053 | 10 / 16 |

Gitaly levels off at about 65 requests per second, with its CPU peaking at 680% while git processes run. Scion serves 370–610 requests per second with its CPU peaking at 82% (and PostgreSQL's at 59%).

### Writes

| Operation | libgit2 (no network, no hooks) | Gitaly (starhub's `CommitFiles`, with hooks) | Scion (commit + land) |
|---|---|---|---|
| Edit one file | **1.4** / 3.5 ms | 327 / 412 ms | 19 / 36 ms |
| One commit of 200 files | 87 ms | 4,762 ms | **37 ms** |
| One 20 MB file | **52 ms** | 716 ms | 326 ms |
| 16 writers, one branch, 160 commits, no retry | ❌ 138 failed (ref locked or moved) | ❌ 150 failed ("reference does not point to expected object"); some requests waited over a minute | ✅ **all 160** (the server rebases), 0.46 s |
| Same, each writer retrying until it succeeds | ✅ all 160, 0.35 s | — | — |
| Create / delete a branch | **0.05 / 0.05 ms** | 305 / 80 ms | 0.9 / 1.3 ms |
| `git push` vs `scn commit` + `land`, 1 / 50 / 500 files | — | 929 / 937 / 1,250 ms | **182 / 207 / 304 ms** |

git's ref update is a compare-and-swap on one file, so concurrent writers to one branch fail unless each retries, and a retry has to rebuild its commit on the new tip. Scion's land does that rebase on the server for every writer.

### Checkout and working copy

| Scenario | git through Gitaly | Scion |
|---|---|---|
| Full checkout | 2.2 s (full history) | **0.83 s** (snapshot) |
| `--depth 1` / `--filter=blob:none` | 1.2 s / 1.7 s | — |
| Sparse checkout of two directories (1,511 files) | 0.83 s (blob:none + sparse-checkout) | **0.11 s** |
| Metadata in the working copy | `.git` 82.8 MB (shallow 38.7, partial 45.6) | **`.scion` 1.1 MB** |
| 10 concurrent checkouts | 5.3 s (shallow) | 5.3 s |
| `status` on a clean tree | **18 ms** | 29 ms |

### Storage and import

| | Gitaly | Scion after the import | Scion after `scion admin repack` |
|---|---|---|---|
| Stored objects | one pack (with its index) | 10,085 (packs of what each commit wrote) | **1 pack** + 3 large files |
| Object bytes | 82 MB | 234 MB | **86 MB**: a 27 MB pack (67,847 objects, 51,262 of them deltas) and 59 MB for three binaries |
| MinIO disk | — | 284 MB | **86 MB** |
| Metadata in PostgreSQL | — | 57 MB | **54 MB** |
| Time | 3.0 s (receives the bundle's pack) | 94 s | + 24 s |

After the write workload, Gitaly's repository takes 87 MB and Scion's objects 92 MB (the workload's commits are in small packs until the next repack).

How the storage got here, all on the same data:

| | MinIO disk | Objects in MinIO |
|---|---|---|
| Every object stored on its own (first comparison) | 546 MB | 67,636 |
| Objects written together share a pack | 283 MB | 10,085 |
| `scion admin repack`, each version against the next newer one at its path | 111 MB | 5 |
| Merged branches' versions stored against the merge, delta chains of 50 | **86 MB** | 5 |

- **The three binaries** are files committed by mistake, with one version each, so nothing can be stored against them: `bin/starhub-server` (30 MB), a debug binary (34 MB) and `GeoLite2-City.mmdb` (63 MB). They take 59 MB on both sides: objects over 8 MB stay loose in Scion, and git cannot delta them either.
- **The rest of the history** takes 27 MB in Scion's pack against about 22 MB in git's (pack and index). By kind:

  | | Scion | git |
  |---|---|---|
  | Files (blobs) | 16.1 MB (with one 2.4 MB file stored loose) | 15.6 MB |
  | Directories (trees) | 6.1 MB | 3.6 MB |
  | Commits / changesets | 4.4 MB | 1.4 MB |
  | Index of the pack | 3.4 MB (in the pack's trailer) | 1.9 MB (`.idx`) |

  A changeset is larger than a git commit because it also lists the files it changed. Storing changesets as deltas against each other saved only 0.2 MB and would slow history reads, so it is not done.
- **The repack's first version left half the trees whole.** It compared each commit with its first parent only, so the last versions on every merged branch had nothing newer to be stored against: with starhub's merge requests, that was every branch. Comparing merges with their other parents too took the pack from 51 MB to 32 MB, and allowing delta chains of 50 (as git does) instead of 16 to 27 MB.
- **PostgreSQL**: a repack rewrites every pack location. The repack now compacts the metadata afterwards: it rewrites the pack locations table in place (64 ms of locking here; skipped for tables over 256 MB or when the lock is not granted within 200 ms) and rebuilds the indexes over object ids and paths concurrently. The database goes from 68 MB to 54 MB, about 8 MB of which is PostgreSQL's own catalog. `path_changes` takes 15 MB, `objects_index` and `pack_locations` 11 MB each.

### Deployment and resources

| | Gitaly + MinIO | Scion + PostgreSQL + MinIO |
|---|---|---|
| Server image | 353 MB | **50 MB** (+ PostgreSQL 159 MB) |
| Restart until serving | 5.7 s | **0.13 s** |
| Memory, idle | Gitaly 194 MiB | Scion **8 MiB** + PostgreSQL 115 MiB |
| Memory, peak under the workload | Gitaly 513 MiB | Scion 717 MiB + PostgreSQL 206 MiB |
| CPU, peak | 682% | 82% (Scion) + 59% (PostgreSQL) |

Scion's peak memory is mostly caches: objects, pack blocks (128 MiB), delta bases (64 MiB) and small packs (64 MiB).

### Conclusions

**Against git itself** (libgit2, no network):

- A Scion request costs at least a round trip to its server, about 1 ms here. On top of that, Scion's reads take 0–3 ms more than libgit2's: file reads, listings, diffs and single commits. History pages are faster than libgit2's (1.6 ms against 1.9 ms) even with the round trip.
- A single small commit is faster in git (1.4 ms against 19 ms); a commit of 200 files is faster in Scion (37 ms against 87 ms).
- Concurrent commits to one branch fail in git unless the writer retries; Scion's land rebases them on the server.
- After a repack, Scion stores this history's objects in 86 MB against git's 82 MB, plus 54 MB of metadata in PostgreSQL.

**Against Gitaly as starhub runs it**, Scion is far ahead on every read and write. Most of that gap is Gitaly's own overhead (a git process per request, its gRPC layer, starhub's hooks and extra work in starhub's client), not git's storage: Gitaly's raw directory listing takes 52 ms where libgit2 takes 0.01 ms.

Scion is also ahead on sparse checkout, working copy size, and deployment size and startup. On contents, listings, diffs and archives it matches git exactly.

**To fix before starhub adopts it, in order:**

1. **Git-compatible history attribution.** Last commit per entry, blame and path history need to follow merges into the merged branch, as git does.
2. **Memory under load.** 717 MiB at peak: make the caches' sizes configurable and bounded together.
3. **Storage.** Run repack automatically (now an admin command), and shrink the metadata in PostgreSQL.
4. **Import speed for migrations.** 94 s plus a repack, against 3 s for Gitaly; a server-side import is roadmap item 2.6.

### Limits of the comparison

- One machine and one run per configuration; some operations (archive, repository size, the large file) have small samples.
- The libgit2 column pays no network or server cost. A git server built on libgit2 would add at least a round trip to each of its numbers.
- Where libgit2 has no API, its column measures the git command line, including starting a process.
- Repository size means different things on the two sides (starhub's `GetRepoSize` reports 935 MB, Scion's `GetTreeStats` the 102 MB of files on main), so it is not compared.
- Gitaly's import receives a ready-made pack, while Scion's rewrites every commit. The import times compare migration paths, not storage engines.

## Reproducing

See [`bench/starhub/README.md`](../../bench/starhub/README.md). Both scripts start from empty data:

- `fulltest.py` runs round 1 against a Lite server.
- `compare.py` builds the Docker setup, imports into both systems, and runs round 2.
