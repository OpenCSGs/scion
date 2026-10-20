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
| Import directory | 365.0 | 32.8 | **30.6** | 3,364.4 | 413.5 | **354.9** |
| Commit one changed file | 28.4 | 29.2 | **13.9** | 250.8 | 142.5 | **35.5** |
| Full clone / checkout | 243.0 | 115.8 | **52.2** | 2,361.9 | 1,047.3 | **589.4** |
| One-directory clone / checkout | 60.3 | 25.0 | **23.1** | 172.9 | 123.1 | **49.5** |

CSG Scion is the fastest of the three in every cell: 2–12× faster than Git, and against CarterT27/scion 1.1–2.5× faster for checkouts, 2.1–4× for commits and 7–17% for imports.

- **Import:** Git runs `init`, `add -A` and `commit`. CSG Scion uploads the files in batches of 256 and creates a parentless changeset and a ref through the write API.
- **Commit:** change one byte in one file of the full working copy, then time Git's `commit -a` or CSG Scion's working-copy commit, including the status scan and the upload. Neither side pushes or lands.
- **Full checkout:** Git's `clone --no-local`; CSG Scion's working-copy clone.
- **One-directory checkout:** Git's `clone --no-local --filter=blob:none --sparse` and `sparse-checkout set dir0000`; CSG Scion's clone of `dir0000` only.

Both implementations run an in-process server with in-memory metadata and objects; CSG Scion's objects go through the same zstd compression and packing as on disk or S3. Both servers require token authentication: the client exchanges a long-lived token for short-lived access tokens and every request carries one. CarterT27/scion's also uses TLS, which CSG Scion's in-process server does not (deployments terminate TLS in front of it); without authentication CSG Scion's times change by less than 4 ms. Fixture writes and checks of the checked-out files are outside the timers.

```sh
(cd bench/openscion && cargo build --release)
bench/openscion/target/release/openscion-bench 100 7 objects auth      # 1,000 files, 7 runs
bench/openscion/target/release/openscion-bench 1000 5 objects auth     # 10,000 files, 5 runs
```

### Real repositories

[pingdotgg/t3code](https://github.com/pingdotgg/t3code) at the snapshot CarterT27/scion's README measures: 5,573 commits and 28,068 files. CSG Scion ran on PostgreSQL and MinIO in Docker on the M5 above, with Git on a local bare repository. The M4 Pro columns are CarterT27/scion's published numbers, from its README (CockroachDB and MinIO), as it was not rerun here.

| Scenario | Git, M5 | CSG Scion, M5 | Git, M4 Pro | CarterT27, M4 Pro |
| --- | ---: | ---: | ---: | ---: |
| Full checkout (28,068 files) | 2.13 s | 2.20 s | 2.90 s | 14.96 s |
| Checkout of `apps/web` (1,521 files) | 0.173 s | 0.178 s | 0.21 s | 0.97 s |
| Clean status | 98 ms | **75 ms** | — | — |
| Full history: Git's clone, Scion's import | 3.96 s | 103 s + 42 s repack | 3.56 s | 23 min 18 s |
| Storage of the history | 557 MB pack | 247 MB in MinIO + 90 MB in PostgreSQL | — | — |

Against Git on the same machine, CSG Scion's full and `apps/web` checkouts both take 1.03× Git's time; CarterT27/scion's take 5.2× and 4.6×. Its clean status takes 0.77× Git's. A Scion import builds and stores every commit on the server, so it is compared with the other implementation's import rather than with Git's clone; CarterT27/scion publishes no status or storage numbers. Every checkout is checked file by file against Git, and Git's pack is the one cloned from GitHub, not repacked.

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

## Deployment

The image `opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion` (tags `latest` and `0.1.0`) is built for `linux/amd64` and `linux/arm64`, so it runs on Linux servers and in Docker Desktop on Apple silicon and Intel Macs. It holds the server, `scion`, and the client, `scn`, runs as an unprivileged user, and requires authentication by default.

**One node (Lite: SQLite and a local directory).**

```sh
docker run -d --name scion -p 7080:7080 -v scion-data:/data opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion:latest

# The first administrator: a policy (admins manage everything; anyone signed in can read, write and land)
docker exec scion sh -c 'printf "%s\n" "[[rule]]" "subjects = [\"group:admins\"]" "repos = [\"*\"]" "actions = [\"admin\"]" \
  "[[rule]]" "subjects = [\"*\"]" "repos = [\"*\"]" "actions = [\"land\"]" > /data/acl.toml \
  && scion admin policy set --lite /data/acl.toml'
# and a token, shown only this once
docker exec scion scion admin token create --lite --subject admin --group admins

scn --endpoint http://localhost:7080 login --token scn_pat_…    # the policy takes effect within 30 s
scn --endpoint http://localhost:7080 repo create mono
```

**Production (Standard: PostgreSQL and S3-compatible storage).** Point the server at both through environment variables, and run the admin commands above without `--lite`:

```sh
docker run -d --name scion -p 7080:7080 \
  -e SCION_META=postgres://scion:PASSWORD@db:5432/scion \
  -e SCION_BLOBS=s3://scion-objects -e SCION_S3_ENDPOINT=https://s3.example.com \
  -e SCION_S3_ACCESS_KEY_ID=… -e SCION_S3_SECRET_ACCESS_KEY=… \
  opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion:latest --log-format json server --listen 0.0.0.0:7080 --auth-required
```

Several nodes behind a load balancer share one database and bucket ([clusters](docs/guide/cluster.md)); a regional cache runs from the same image as `scion cache` ([caching](docs/guide/cache.md)). To try everything on one machine, [`deploy/compose`](deploy/compose/docker-compose.yml) starts PostgreSQL, SeaweedFS and the server without authentication. The server refuses to listen beyond loopback without `--auth-required` unless `--allow-anonymous` is given. [Deployment](docs/guide/deploy.md) and [authentication](docs/guide/auth.md) have the details: ports, health checks, OIDC login and external identity providers such as starhub-server.

## Documentation

- **Using Scion:** [deployment](docs/guide/deploy.md) · [command tour](docs/guide/cli.md) · [coming from git](docs/guide/coming-from-git.md) · [importing from git](docs/guide/git-import.md) · [sparse checkout](docs/guide/sparse-checkout.md) · [repositories and branches](docs/guide/repos.md) · [archives](docs/guide/archive.md) · [directory listings](docs/guide/browse.md) · [git SHAs](docs/guide/git-sha.md)
- **Running a server:** [object storage](docs/guide/object-storage.md) · [clusters and landing](docs/guide/cluster.md) · [caching](docs/guide/cache.md) · [authentication and access control](docs/guide/auth.md) · [redaction and fsck](docs/guide/redaction.md) · [ref events](docs/guide/events.md) · [metrics and tracing](docs/guide/observability.md)
- **Design:** [server](docs/design/rust-server-architecture.md) · [client](docs/design/rust-client-architecture.md) · [starhub / CSGLite integration](docs/design/starhub-integration.md) · object format [`SPEC.md`](SPEC.md) · protocol [`proto/scion/v1/`](proto/scion/v1/)

Every guide and design document has a Chinese version, linked at its top.

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
