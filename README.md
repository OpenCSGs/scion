# Scion

English | [简体中文](README.zh-CN.md)

Server-first source control for monorepos, coding agents and CI.

Scion is a Rust implementation of the design in Anthropic's [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control): history, merges and rebases are computed on the server, clients stay online and fetch immutable objects on demand, and a checkout holds only the paths a job needs. It ships a server, `scion`, and a client, `scn`, over a language-neutral protocol, and is built by OpenCSG for [starhub-server](https://github.com/OpenCSGs/csghub-server) and CSGLite. It is not Anthropic's implementation and is not affiliated with Anthropic.

![Scion standalone architecture](docs/design/images/arch-01-standalone.en.png)


## Benchmarks

The [benchmark runner](bench/openscion/src/main.rs) compares CSG Scion with Git and with [CarterT27/scion](https://github.com/CarterT27/scion), another implementation of the same design, on the four synthetic workloads used by [OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md). All three ran on one machine: an Apple M5 with 16 GiB RAM, macOS 26.5.2 and Git 2.54.0. CarterT27/scion was built from its `main` at the time and run with its own runner.

Each file contains 4 KiB of pseudorandom bytes, spread across 10 directories. The 1,000-file case uses seven runs and the 10,000-file case uses five. All times are median milliseconds; lower is better.

| Scenario | Git, 1k files | CarterT27, 1k | CSG Scion, 1k | Git, 10k files | CarterT27, 10k | CSG Scion, 10k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Import directory | 375.1 | 32.8 | **29.6** | 3,342.0 | 413.5 | **362.1** |
| Commit one changed file | 29.7 | 29.2 | **13.4** | 226.7 | 142.5 | **42.2** |
| Full clone / checkout | 251.3 | 115.8 | **50.7** | 2,316.6 | 1,047.3 | **568.3** |
| One-directory clone / checkout | 65.0 | 25.0 | **19.4** | 181.7 | 123.1 | **48.6** |

CSG Scion is the fastest of the three in every cell: 2.2–13× faster than Git, and against CarterT27/scion 1.3–2.5× faster for checkouts, 2.2–3.4× for commits and 11–14% for imports.

- **Import:** Git runs `init`, `add -A` and `commit`. CSG Scion uploads the files in batches of 256 and creates a parentless changeset and a ref through the write API.
- **Commit:** change one byte in one file of the full working copy, then time Git's `commit -a` or CSG Scion's working-copy commit, including the status scan and the upload. Neither side pushes or lands.
- **Full checkout:** Git's `clone --no-local`; CSG Scion's working-copy clone.
- **One-directory checkout:** Git's `clone --no-local --filter=blob:none --sparse` and `sparse-checkout set dir0000`; CSG Scion's clone of `dir0000` only.

Both implementations run an in-process server with in-memory metadata and objects; CSG Scion's objects go through the same zstd compression and packing as on disk or S3. CarterT27/scion's server uses TLS and token authentication, while CSG Scion's runs without either, which costs CarterT27's checkouts a few percent. Fixture writes and checks of the checked-out files are outside the timers.

```sh
(cd bench/openscion && cargo build --release)
bench/openscion/target/release/openscion-bench 100 7      # 1,000 files, 7 runs
bench/openscion/target/release/openscion-bench 1000 5     # 10,000 files, 5 runs
```

### Real repositories

[pingdotgg/t3code](https://github.com/pingdotgg/t3code) at the snapshot CarterT27/scion's README measures: 5,573 commits and 28,068 files. CSG Scion ran on PostgreSQL and MinIO in Docker on the M5 above, with Git on a local bare repository. The M4 Pro columns are CarterT27/scion's published numbers, from its README (CockroachDB and MinIO), as it was not rerun here.

| Scenario | Git, M5 | CSG Scion, M5 | Git, M4 Pro | CarterT27, M4 Pro |
| --- | ---: | ---: | ---: | ---: |
| Full checkout (28,068 files) | 2.65 s | **2.28 s** | 2.90 s | 14.96 s |
| Checkout of `apps/web` (1,521 files) | 0.172 s | 0.178 s | 0.21 s | 0.97 s |
| Clean status | **98 ms** | 153 ms | — | — |
| Full history: Git's clone, Scion's import | 3.96 s | 103 s + 42 s repack | 3.56 s | 23 min 18 s |
| Storage of the history | 557 MB pack | 247 MB in MinIO + 90 MB in PostgreSQL | — | — |

Against Git on the same machine, CSG Scion's full checkout takes 0.86× Git's time and its `apps/web` checkout 1.03×; CarterT27/scion's take 5.2× and 4.6×. Its clean status is 1.6× Git's. A Scion import builds and stores every commit on the server, so it is compared with the other implementation's import rather than with Git's clone; CarterT27/scion publishes no status or storage numbers. Every checkout is checked file by file against Git, and Git's pack is the one cloned from GitHub, not repacked.

- [Report: CarterT27/scion's benchmarks](docs/reports/2026-10-20-openscion-workloads.md), with the method, every number and how to rerun it.
- [Report: starhub-server](docs/reports/2026-10-19-starhub-server.md): a full real-world test on starhub-server's history, compared with starhub's Gitaly + MinIO stack and with libgit2.
- [Component benchmarks](docs/performance.md): the cache, landing, history queries and checkouts at 100,000 files.

## How it works

- **Writes on the server.** A commit is created on the server from the files it changes; `land` moves a branch to it and rebases it on the server when the branch has moved, exactly once, without a global queue. Concurrent writers to one branch all succeed.
- **Fetch on demand.** Clients ask for the trees and files they want by id, so sparse checkout needs no special support: directories outside the scope are never fetched.
- **History without a clone.** Log, diff, blame, ancestry, merge bases, the last commit of each entry and archives are server queries.
- **Three storage tiers.** Lite (SQLite and a local directory), Standard (PostgreSQL and S3-compatible storage) and in-memory for tests. Objects are content-addressed, zstd-compressed and packed; `scion admin repack` stores older versions as deltas against newer ones.
- **Caches.** A regional cache, `scion cache`, and a local daemon, `scn daemon`, serve the same protocol as the server, so clients do not need to know the topology.
- **Git compatibility.** `scn import git` brings in a repository with its history; every commit keeps a git SHA, accepted wherever a revision is.
- **Access control and redaction.** Tokens, OIDC login, external issuers, ACLs by repository, ref and path, and redaction of leaked content without rewriting history.

## Quick start

```sh
# A server in Lite mode (SQLite + a local directory), from source
cargo run -p scion -- server --lite --data-dir ./.scion-data
# or with PostgreSQL + S3-compatible storage: docker compose -f deploy/compose/docker-compose.yml up -d --build

# In another terminal
cargo install --path client/cli
scn repo create mono
scn clone mono && cd mono
echo hello > README.md
scn commit -m "add readme"
scn land                          # move main to the commit; the server rebases if main moved
scn log
```

[A tour of scion and scn](docs/guide/cli.md) goes through the everyday commands, and [coming from git](docs/guide/coming-from-git.md) maps git commands to scn.

## Documentation

- **Using Scion:** [deployment](docs/guide/deploy.md) · [command tour](docs/guide/cli.md) · [coming from git](docs/guide/coming-from-git.md) · [importing from git](docs/guide/git-import.md) · [sparse checkout](docs/guide/sparse-checkout.md) · [repositories and branches](docs/guide/repos.md) · [archives](docs/guide/archive.md) · [directory listings](docs/guide/browse.md) · [git SHAs](docs/guide/git-sha.md)
- **Running a server:** [object storage](docs/guide/object-storage.md) · [clusters and landing](docs/guide/cluster.md) · [caching](docs/guide/cache.md) · [authentication and access control](docs/guide/auth.md) · [redaction and fsck](docs/guide/redaction.md) · [ref events](docs/guide/events.md) · [metrics and tracing](docs/guide/observability.md)
- **Design:** [server](docs/design/rust-server-architecture.md) · [client](docs/design/rust-client-architecture.md) · [starhub / CSGLite integration](docs/design/starhub-integration.md) · object format [`SPEC.md`](SPEC.md) · protocol [`proto/scion/v1/`](proto/scion/v1/)

Most guides and design documents are in Chinese for now.

## Status

M1–M4 are complete: the write path and working copy, history queries, server-side rebase and landing, clusters, and the regional cache and local daemon. M5, production hardening, is in progress; authentication, redaction, observability, ref events, packs and repack are done, and the YugabyteDB tier, Helm and release builds remain. See [milestones](docs/design/milestones.md) for the details and the [roadmap](docs/design/roadmap.md) for larger tasks planned for later.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

[Development](docs/development.md) describes the repository layout, the dependency rules between crates and the testing requirements (at least 90% coverage of new code).

## License

Apache-2.0
