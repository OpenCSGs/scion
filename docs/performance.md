# Performance

The comparison with Git and CarterT27/scion is in the [README](../README.md#benchmarks). This page lists the benchmarks of Scion's own components and the real repositories it was tested on.

Benchmarks run on one machine (Apple M5, 10 cores) with objects in memory; client, cache and server share the machine. Each row links the benchmark so you can rerun it (`cargo test --release -p <crate> --test bench -- --ignored --nocapture`).

| Area | Workload | Result | Target |
|---|---|---|---|
| Cache hit rate ([`server/cache/tests/bench.rs`](../server/cache/tests/bench.rs)) | 200 clients check out the same 20,000 objects (1–4 KiB) from a cold cache | 99.5% of reads served without L3; each object fetched from L3 exactly once | ≥ 99% |
| Cache throughput (same) | 64 clients, warm cache, batches of 64 objects over gRPC | 1.53M objects/s per node | ≥ 200k/s |
| Cache throughput (same) | 64 clients, warm cache, one object per request | 164k requests/s per node | — |
| Checkout ([`client/daemon/tests/bench.rs`](../client/daemon/tests/bench.rs)) | `scn clone` of 100,000 files in 1,000 directories | Through a warm daemon 5.3 s; creating the same files without any network takes 5.4 s on this disk, so the daemon adds no measurable overhead | < 5 s, disk permitting |
| Landing ([`server/land/tests/bench.rs`](../server/land/tests/bench.rs)) | 2,000 lands on one ref from 200 concurrent clients | PostgreSQL 885/s (p99 379 ms); SQLite 1,921/s (p99 173 ms) | ≥ 200/s, p99 < 500 ms |
| History queries ([`server/query/tests/bench.rs`](../server/query/tests/bench.rs)) | 100,000 commits: log, diff, ancestry, merge-base, blame | p99 < 20 ms on SQLite and PostgreSQL (cold blame 13.6 ms, `log --path` 15.6 ms) | p99 < 100 ms |
| Directory listings (same benchmark) | Last commit of every entry in a 100-entry root, and repository stats, at 100,000 commits | PostgreSQL: cold p50 33 ms (p99 93 ms); after a land with the parent cached p99 2.4 ms; cached p99 < 5 ms. Repository stats cold 131 ms, next commit 3.1 ms | p99 < 100 ms |
| Real repository, Standard ([`bench/starhub`](../bench/starhub/README.md)) | starhub-server (6,338 commits, two branches, a tag) on PostgreSQL + MinIO in Docker | Import 94 s, `scion admin repack` 24 s: 86 MB of MinIO disk (546 MB when every object was stored on its own; Gitaly's repository 82 MB) plus 54 MB of metadata in PostgreSQL. Cold reads 1–4 ms, history page 1.6 ms. [Compared with Gitaly and libgit2](reports/2026-10-19-starhub-server.md) | — |
| OpenScion workloads ([`bench/openscion`](../bench/openscion/README.md)) | 1,000 and 10,000 files: import, commit one file, full and one-directory checkout, against Git and CarterT27/scion on one machine | 10k files: import 362 ms, commit 42 ms, full checkout 568 ms, one directory 49 ms; faster than both in all 8 cells. [Report](reports/2026-10-20-openscion-workloads.md) | — |
| Real repository, Standard (same) | pingdotgg/t3code (5,573 commits, 28,068 files) on PostgreSQL + MinIO | Import 103 s + repack 42 s; 247 MB of MinIO (GitHub pack 557 MB) and 90 MB of PostgreSQL. Full checkout 2.20 s (Git 2.13 s), `apps/web` 0.18 s (Git 0.17 s), status 75 ms (Git 98 ms) | — |
| Real repository (`~/git/starhub-server`, Lite, this machine) | Import 6,312 commits (2,836 merges, 4,663 files); clone; queries | Import 57 s; object bytes 227 MB with zstd (1,007 MB uncompressed; git pack 392 MB); clone 0.37 s; status 23 ms; log, blame, show, diff 8–50 ms. Files, git SHAs, blame, diff and archive match git | — |
