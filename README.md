# Scion

English | [简体中文](README.zh-CN.md)

A server-first source control system: history, merges and rebases are computed on the server, and clients stay online and fetch data on demand. The protocol is language-neutral; the project ships a Rust server, `scion`, and a Rust client, `scn`.

![Scion standalone architecture](docs/design/images/arch-01-standalone.en.png)

<sub>Diagram sources: [`arch-01-standalone.en.svg`](docs/design/images/arch-01-standalone.en.svg) (English) · [`arch-01-standalone.svg`](docs/design/images/arch-01-standalone.svg) (Chinese). The diagram includes planned components (L2 cache, deriver, git-gateway, …); see [Status](#status) for what is implemented today.</sub>

## Documentation

**Guides**
- [Deployment](docs/guide/deploy.md) (Chinese): Docker image, docker compose with PostgreSQL and S3-compatible storage
- [Test report: starhub-server](docs/reports/2026-10-19-starhub-server.md): a full real-world test on starhub-server's history, and a side-by-side comparison with starhub's Gitaly + MinIO stack and with libgit2
- [Coming from git](docs/guide/coming-from-git.md): git commands and their scn equivalents, revision syntax (`HEAD~1`, `A..B`)
- [Repositories and branches](docs/guide/repos.md) (Chinese): create, list, fork, delete and restore repositories; manage branches
- [Ref events](docs/guide/events.md) (Chinese): an ordered, gap-free feed of ref changes; webhook and NATS JetStream delivery; the pre-land check
- [Archives](docs/guide/archive.md) (Chinese): download a revision or a directory as tar.gz, zip or tar with `scn archive`
- [Directory listings](docs/guide/browse.md) (Chinese): the last commit of every entry in a directory, file counts and sizes
- [Git-compatible commit SHAs](docs/guide/git-sha.md) (Chinese): a 40-hex git SHA-1 for every commit, accepted wherever a revision is
- [Importing from git](docs/guide/git-import.md) (Chinese): import an existing git repository with its full history
- [Sparse checkout](docs/guide/sparse-checkout.md) (Chinese): check out only the directories you need; the rest is never fetched
- [Object storage](docs/guide/object-storage.md) (Chinese): SeaweedFS, MinIO and other S3-compatible services; provider detection and startup checks
- [Clusters and landing](docs/guide/cluster.md) (Chinese): land semantics (rebase, exactly once), multi-node mode with `--advertise-url`, failover and overload protection
- [Caching](docs/guide/cache.md) (Chinese): the regional cache `scion cache` (sharding, hedging, warm-up) and the local daemon `scn daemon`
- [Metrics and tracing](docs/guide/observability.md) (Chinese): Prometheus metrics, OpenTelemetry tracing, useful queries
- [Authentication and access control](docs/guide/auth.md) (Chinese): tokens, ACL policy, OIDC login, external issuers, OPA-compatible authorization, starhub-server integration
- [Redaction and data checks](docs/guide/redaction.md) (Chinese): removing leaked content (redact, purge), `scion admin fsck`, `scion admin lease ls`

**Design and specifications** (Chinese)
- Design docs: [server](docs/design/rust-server-architecture.md) · [client](docs/design/rust-client-architecture.md) · [starhub / CSGLite integration](docs/design/starhub-integration.md) · [roadmap](docs/design/roadmap.md) (large planned tasks that are not needed yet, such as virtualized checkout)
- Object format: [`SPEC.md`](SPEC.md), with golden test vectors in [`testdata/vectors/`](testdata/vectors/)
- Protocol: [`proto/scion/v1/`](proto/scion/v1/)

## Status

**M5 (production hardening) is in progress.** Done so far:
- **Authentication and access control:**
  - tokens: self-issued ES256 access tokens, PATs, and refresh tokens;
  - external identities, standards only: JWKS or HS256 issuers, RFC 7662 introspection and an OPA-compatible authorization webhook. starhub-server is supported through configuration alone;
  - ACL rules by subject, repo, action, ref and path;
  - `scn login` with the OIDC device flow, credentials stored in the keychain.
- **Redaction:**
  - redact leaked file contents: object ids and history stay the same, reads return `REDACTED`;
  - the regional cache and the local daemon stop serving them within 10 seconds;
  - land never merges redacted content into new commits;
  - purge deletes the bytes after a grace period, once every repository that holds the object has redacted it.
- **Operations:** `scion admin fsck` checks refs, commits, trees and files against the object index and object store; `scion admin lease ls` lists leases.
- **Sparse checkout:** `scn clone --sparse a/,b/` and `scn sparse add|rm|set|disable`. Subtrees outside the scope are never fetched; commits, lands and rebases leave them untouched on the server.

- **Observability and limits:**
  - Prometheus metrics on `--metrics-listen`: per-RPC counts, codes and latency; land batches and latency; cache hit rates at L2 and L3; rejected requests;
  - OpenTelemetry tracing over OTLP, continuing the caller's `traceparent` across land forwarding and cache fetches;
  - per-caller rate and concurrency limits (`--tenant-rate`, `--tenant-concurrency`);
  - deadlines: the smaller of the client's `grpc-timeout` and `--max-request-timeout`, passed on to the leader and to the servers.

- **Repositories and branches:** `RepoService` (create, list, update, soft-delete with a restore window, fork) and branch create / move / delete, with `scn repo` and `scn branch`.
- **Deployment:** a Docker image with `scion` and `scn`, and a docker compose setup with PostgreSQL and SeaweedFS.
- **Ref events:**
  - every ref change is written in the same transaction as an ordered event;
  - consumers poll `EventService.ListRefEvents` by sequence number;
  - or the server pushes the events, with commit and path-level details, to a webhook (HMAC-signed) or NATS JetStream, at least once and in order.
- **Pre-land check:** before a land or a branch write, the server can POST the commits it would introduce to an external service, which allows or rejects them (fail-closed by default).

Still to come in M5: YugabyteDB, Helm and chaos testing.

**M4 (cache layer) is complete**:
- **Regional cache `scion cache` (L2):**
  - storage: foyer memory plus NVMe;
  - rendezvous sharding across nodes, so the region fetches each object from the server once;
  - single-flight coalescing, plus hedged reads and circuit breakers toward the servers;
  - warm-up from WatchRefs that only walks changed trees.
- **Local daemon `scn daemon` (L1):**
  - a verified on-disk object cache;
  - all other RPCs forwarded transparently;
  - prefetch of refs the working copy follows;
  - started on demand by `scn`.
- **WatchRefs:** streams ref changes to subscribers (snapshot first, then each advance), resumable after disconnects.
- **Server read path:**
  - in-process object cache;
  - coalesced, concurrency-limited backend reads;
  - `--read-endpoint` advertises caches to clients.

The cache serves 99.5% of reads without the server and 1.53M objects/s per node; see [Performance](#performance).

Earlier milestones:
- M3 added server-side rebase, exactly-once lands, batched landing (885 lands/s on one ref), clusters and overload protection.
- M2 added history queries (p99 under 20 ms on 100k commits).
- M1 added the write path, Lite mode, the `scn` working copy and git import.

The remaining M5 work: the YugabyteDB tier, Helm and release builds.

## Performance

Benchmarks run on one machine (Apple M5, 10 cores) with objects in memory; client, cache and server share the machine. Each row links the benchmark so you can rerun it (`cargo test --release -p <crate> --test bench -- --ignored --nocapture`).

| Area | Workload | Result | Target |
|---|---|---|---|
| Cache hit rate ([`server/cache/tests/bench.rs`](server/cache/tests/bench.rs)) | 200 clients check out the same 20,000 objects (1–4 KiB) from a cold cache | 99.5% of reads served without L3; each object fetched from L3 exactly once | ≥ 99% |
| Cache throughput (same) | 64 clients, warm cache, batches of 64 objects over gRPC | 1.53M objects/s per node | ≥ 200k/s |
| Cache throughput (same) | 64 clients, warm cache, one object per request | 164k requests/s per node | — |
| Checkout ([`client/daemon/tests/bench.rs`](client/daemon/tests/bench.rs)) | `scn clone` of 100,000 files in 1,000 directories | Through a warm daemon 5.3 s; creating the same files without any network takes 5.4 s on this disk, so the daemon adds no measurable overhead | < 5 s, disk permitting |
| Landing ([`server/land/tests/bench.rs`](server/land/tests/bench.rs)) | 2,000 lands on one ref from 200 concurrent clients | PostgreSQL 885/s (p99 379 ms); SQLite 1,921/s (p99 173 ms) | ≥ 200/s, p99 < 500 ms |
| History queries ([`server/query/tests/bench.rs`](server/query/tests/bench.rs)) | 100,000 commits: log, diff, ancestry, merge-base, blame | p99 < 20 ms on SQLite and PostgreSQL (cold blame 13.6 ms, `log --path` 15.6 ms) | p99 < 100 ms |
| Directory listings (same benchmark) | Last commit of every entry in a 100-entry root, and repository stats, at 100,000 commits | PostgreSQL: cold p50 33 ms (p99 93 ms); after a land with the parent cached p99 2.4 ms; cached p99 < 5 ms. Repository stats cold 131 ms, next commit 3.1 ms | p99 < 100 ms |
| Real repository, Standard ([`bench/starhub`](bench/starhub/README.md)) | starhub-server (6,338 commits, two branches, a tag) on PostgreSQL + MinIO in Docker | Import 94 s, `scion admin repack` 24 s: 86 MB of MinIO disk (546 MB when every object was stored on its own; Gitaly's repository 82 MB) plus 54 MB of metadata in PostgreSQL. Cold reads 1–4 ms, history page 1.6 ms. [Compared with Gitaly and libgit2](docs/reports/2026-10-19-starhub-server.md) | — |
| Real repository (`~/git/starhub-server`, Lite, this machine) | Import 6,312 commits (2,836 merges, 4,663 files); clone; queries | Import 57 s; object bytes 227 MB with zstd (1,007 MB uncompressed; git pack 392 MB); clone 0.37 s; status 23 ms; log, blame, show, diff 8–50 ms. Files, git SHAs, blame, diff and archive match git | — |

## Quick start

```sh
# Option 1: docker compose with PostgreSQL + S3-compatible storage (see docs/guide/deploy.md)
docker compose -f deploy/compose/docker-compose.yml up -d --build

# Option 2: a server in Lite mode (SQLite + a local directory, persisted), from source
cargo run -p scion -- server --lite --data-dir ./.scion-data

# Other backends: --meta mem|sqlite://PATH|postgres://…  --blobs mem|DIR|s3://bucket/prefix
# S3 settings: SCION_S3_ENDPOINT, SCION_S3_ACCESS_KEY_ID, SCION_S3_SECRET_ACCESS_KEY, SCION_S3_REGION,
# SCION_S3_PROVIDER=auto|minio|seaweedfs|aws|generic (SeaweedFS and MinIO are tested in CI; see docs/guide/object-storage.md)

# In another terminal: install scn and check the connection
cargo install --path client/cli
scn doctor
scn config set user.name "Ada Lovelace"       # default commit author (falls back to git's user.name / user.email)
scn config set user.email ada@example.com
scn repo create mono                # repositories and branches: scn repo / scn branch (see docs/guide/repos.md)

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
scn archive -o mono.tar.gz          # download a snapshot as tar.gz / zip / tar (see docs/guide/archive.md)

# scn talks to the server through a local daemon (scn daemon), started on demand; SCION_NO_DAEMON=1 opts out
scn daemon status

# A regional cache in front of one or more servers; advertise it with scion server --read-endpoint
# (see docs/guide/cache.md)
cargo run -p scion -- cache --upstream http://127.0.0.1:7080 --listen 127.0.0.1:7090 --warm mono:main

# Import an existing git repository with its history, authors and timestamps
# (see docs/guide/git-import.md)
scn import git ~/src/my-repo --repo mono

# After upgrading a server that already has data, before starting it: repair lineage and backfill derived indexes (idempotent)
cargo run -p scion -- admin reindex --lite --data-dir ./.scion-data
```

## Repository layout

```
proto/scion/v1/      protocol definitions (.proto), language-neutral; buf lint / breaking checks
SPEC.md              object model and canonical encoding
testdata/vectors/    golden test vectors every implementation must pass

common/              shared by server and client (may depend only on common)
  protocol/          scion-proto       Rust code generated from the protos
  object/            scion-object      ObjectId, canonical encoding, Tree / Changeset (pure, no IO)
  flight/            scion-flight      single-flight request coalescing (server caches and the daemon)

server/              server side (may depend on common and lower server crates)
  obs/               scion-obs         Prometheus metrics, OpenTelemetry tracing, gRPC deadlines
  store/             scion-store       MetaStore / BlobStore traits, in-memory backend, conformance suite
  store-sql/         scion-store-sql   SQLite / PostgreSQL metadata store
  store-blob/        scion-store-blob  local directory / S3-compatible object store
  auth/              scion-auth        tokens, external issuers (JWKS / HS256), introspection, OIDC, ACL
  tree/              scion-tree        incremental tree building
  merge/             scion-merge       server-side rebase: replay a delta onto a newer tree with three-way merge
  land/              scion-land        changeset creation, stack rebase, LandCoordinator, leases
  query/             scion-query       history queries (log / diff / blame / merge-base) and reindexing
  cache/             scion-cache       regional object cache (scion cache): foyer, rendezvous, hedging, warm-up
  api/               scion-api         gRPC services and server bootstrap
  scion/             scion             server binary (scion server / scion cache / scion admin)

client/              client side (may depend on common and client, never on server)
  sdk/               scion-client      Rust SDK
  wc/                scion-wc          working copy (clone / status / commit / land / sync)
  daemon/            scion-daemon      local L1 cache (scn daemon): object cache, transparent forwarding, prefetch
  gitimport/         scion-gitimport   git history import
  cli/               scion-cli         the scn binary

xtask/               maintenance tasks (cargo xtask layers: dependency direction check)
deploy/dev/          local development dependencies (SeaweedFS, PostgreSQL)
docs/guide/          user guides
docs/design/         design docs and diagrams
```

Directory names describe location and role; crate names carry a `scion-` prefix to avoid clashing with crates.io packages such as `object`. Dependency direction is enforced in CI by `cargo xtask layers`.

## Development

```sh
cargo test --workspace                                    # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask layers                                        # dependency direction check
cargo llvm-cov --workspace                                # coverage (cargo install cargo-llvm-cov)
SCION_BLESS=1 cargo test -p scion-object --test vectors   # regenerate golden vectors after adding some
docker compose -f deploy/dev/docker-compose.yml up -d     # SeaweedFS (S3) + PostgreSQL
```

**Testing requirements** ([server design doc §12.1](docs/design/rust-server-architecture.md)): every function with logic has unit tests covering the normal path, edge cases and every error branch. Core crates need at least 90% line coverage, and new or changed code in a pull request needs at least 90%; CI enforces both.

## License

Apache-2.0
