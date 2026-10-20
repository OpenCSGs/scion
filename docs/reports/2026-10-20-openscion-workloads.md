# Scion against CarterT27/scion and Git, on their benchmarks

*2026-10-20 · Scion `main` at the time of writing*

[CarterT27/scion](https://github.com/CarterT27/scion) is another implementation of Anthropic's Scion design. Its README reports two benchmarks: the four synthetic workloads of [OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md), and a real repository (pingdotgg/t3code). This report runs both on this Scion. The synthetic workloads were run for all three systems on one machine; for t3code, CarterT27's numbers come from its README, and the two implementations are compared by their ratio to Git.

The scripts are in [`bench/openscion/`](../../bench/openscion/README.md).

## Summary

- **Synthetic workloads: fastest of the three in all 8 cells.** With the same machine, fixture and timers:
  - against Git: 2.2–13× faster;
  - against CarterT27: 1.8–2.3× faster for full checkouts, 1.3–2.5× for one-directory checkouts, 2.2–3.4× for commits and 11–14% for imports.
- **t3code, 28,068 files**:
  - **checkout: on par with Git.** The full checkout takes 2.28 s against Git's 2.65 s; the sparse checkout of `apps/web` takes 0.178 s against 0.172 s. CarterT27 reports 5.2× and 4.6× Git's time for the same checkouts.
  - **import: 103 s plus a 42 s repack.** CarterT27 reports 23 min 18 s.
  - **status is 1.6× Git's time.** Scanning a 28k-file working copy is the slow spot.
- **Two bugs found and fixed on the way**, neither of which starhub-server's history hits:
  - the importer failed on a commit that removes a submodule (`5109efb`);
  - an empty file in a large pack could not be read (`4a2bf9b`).

## Setup

- **Machine:** Apple M5, 10 cores, 16 GiB, macOS 26.5.2, Git 2.54.0. CarterT27's README numbers are from an Apple M4 Pro with 24 GiB.
- **Synthetic workloads:** both implementations run in process over loopback gRPC, with in-memory metadata and objects. Each run uses a fresh fixture, server and working copies, and the fixture writes and checks are outside the timers.
  - CarterT27: its own runner (`cargo run --release -p scion-conformance --example benchmark`), built from `main` at the time of writing.
  - This Scion: `bench/openscion`, the same workloads and timers. Objects go through the object store in memory, with zstd compression and packs, as on disk or S3.
  - Differences: CarterT27's server uses TLS and token authentication; ours runs without TLS (deployments put it behind a proxy) and without authentication. CarterT27's import creates its changeset in an import mode, ours through the ordinary write API.
- **t3code:** pingdotgg/t3code at `bf8ffe81a63c`, the snapshot CarterT27 measured: 5,573 commits, 28,068 files, submodules left out on both sides.
  - Scion: the server, PostgreSQL 16 and MinIO of [`bench/starhub`](../../bench/starhub/README.md), in Docker, with the `scn` command line, the server restarted before the checkouts.
  - Git: a local bare repository, as in CarterT27's script.
  - The checkouts and commands follow CarterT27's `scripts/benchmark-git.py`: a first checkout of each kind, then five more in random order, each checked file by file (SHA-1 and executable bit); two warmups and seven samples for each command.

## Synthetic workloads

10 directories of 4 KiB pseudorandom files. Median milliseconds; 7 runs at 1,000 files, 5 at 10,000.

| Scenario | Git, 1k | CarterT27, 1k | Scion, 1k | Git, 10k | CarterT27, 10k | Scion, 10k |
|---|---:|---:|---:|---:|---:|---:|
| Import a directory | 375 | 32.8 | **29.6** | 3,342 | 414 | **362** |
| Commit one changed file | 29.7 | 29.2 | **13.4** | 227 | 143 | **42.2** |
| Full checkout | 251 | 116 | **50.7** | 2,317 | 1,047 | **568** |
| One-directory checkout | 65.0 | 25.0 | **19.4** | 182 | 123 | **48.6** |

The Git column is from this runner's runs. CarterT27's runner measured Git within 6% of it on every row but the 10k commit (192 ms against 227 ms; Git's commit varies from run to run, as CarterT27's README also notes). With objects kept in memory as they are (no compression, no packs), this Scion's imports take 24 ms and 283 ms; the other rows change by less than 5%.

CarterT27's README, measured on an M4 Pro, has Git ahead for the 10k commit (167 ms against 204 ms) and the 10k one-directory checkout (160 ms against 173 ms). On this machine its implementation is ahead of Git on both.

## t3code

| | Git | Scion | Scion ÷ Git | CarterT27 (README) | CarterT27 ÷ Git |
|---|---:|---:|---:|---:|---:|
| Full checkout (28,068 files) | 2.65 s | **2.28 s** | **0.86** | 14.96 s (Git 2.90 s) | 5.2 |
| Checkout of `apps/web` (1,521 files) | 0.172 s | 0.178 s | 1.03 | 0.97 s (Git 0.21 s) | 4.6 |
| First checkout after a restart, full / `apps/web` | 2.66 / 0.222 s | 2.96 / 0.186 s | | | |
| Clean status | 98 ms | 153 ms | 1.6 | | |
| Status with one edited file | 98 ms | 154 ms | 1.6 | | |
| Clean status, `apps/web` only | 11.8 ms | **8.7 ms** | 0.74 | | |
| 100 first-parent history entries | 6.5 ms | 10.7 ms | 1.6 | | |
| Read README.md | 5.9 ms | 11.8 ms | 2.0 | | |
| Import the full history | — | **103 s** + 42 s repack | | 23 min 18 s | |

- Scion's sparse checkout also writes the 17 files at the repository's root (1,538 files); Git's sparse pattern here is `apps/web` only.
- The command rows include starting `scn`, a few milliseconds; CarterT27's synthetic numbers exclude its process start.

Storage after the import and repack:

| | Git | Scion |
|---|---|---|
| Objects | 557 MB (the pack as cloned from GitHub) | **247 MB** of MinIO, 144,169 objects in 3 packs (100,307 as deltas) and 7 large files |
| Metadata | — | 89.5 MB of PostgreSQL |

GitHub's pack is not packed as tightly as `git gc --aggressive` would, so the Git column is an upper bound.

## Limits

- One machine, and for t3code one import; CarterT27's t3code numbers come from another machine and its own setup (CockroachDB, a token, TLS), so only the ratios to Git compare.
- The synthetic Scion runs differ from CarterT27's in TLS and authentication, which cost both checkouts a few percent.
- CarterT27's concurrent CI workloads (`scripts/benchmark-scaling.py`) are not reproduced here.
