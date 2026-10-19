# Tests on starhub-server

Scripts behind [docs/reports/2026-10-19-starhub-server.md](../../docs/reports/2026-10-19-starhub-server.md). They expect a checkout of starhub-server next to this repository. Set `STARHUB_DIR` to use one somewhere else.

## Round 1: full real-world test (Lite tier)

```sh
cargo build --release -p scion -p scion-cli     # from the repository root
python3 bench/starhub/fulltest.py               # results in bench/starhub/full/results.json
```

The script:

1. starts a fresh Lite server;
2. imports main, two branches and a tag;
3. runs 75 checks: reading, history, a 3,000-file change, two developers, conflicts, branches, undo, sparse checkout, large files, fsck and a restart.

Each check compares Scion's answer with git's where possible.

## Round 2: side by side with Gitaly + MinIO, and with libgit2

| File | What |
|---|---|
| `docker-compose.yml`, `gitaly.config.toml` | Gitaly v16.4.6 and MinIO (as in starhub-server's `docker-compose-gitaly.yml`); scion server with PostgreSQL 16 and the same MinIO |
| `gitalybench/` | Go. It drives Gitaly through **starhub's own client** (`builder/git/gitserver/gitaly`), plus raw Gitaly RPCs for blame, path history, bundle import, and the `_raw` rows (listing and single commit without starhub's extra work). `serve` runs a git smart HTTP proxy built from starhub's `InfoRefsResponse` / `UploadPack` / `ReceivePack`, and a stub of starhub's internal hook API |
| `gitbench/` | Rust, in Docker next to Gitaly. The same workload directly on a bare clone of the bundle through libgit2, with the git command line where libgit2 has no API: git's lower bound, with no server or network |
| `scionbench/` | Rust. The same workload through Scion's SDK |
| `makeplan.py` | The shared workload. Every revision is a git SHA, so both sides answer the same requests |
| `compare.py` | Resets both systems and imports into them. Then it measures deployment, storage, checkout, the API workload (sampling `docker stats`) and push vs land, and compares the three systems request by request. Each server restarts before its workload |

To run it:

```sh
docker build -t scion:compare .                   # from the repository root
(cd bench/starhub/gitalybench && go build -o gitalybench .)
(cd bench/starhub/scionbench && cargo build --release)
docker build -t gitbench bench/starhub/gitbench   # --build-arg DEBIAN_MIRROR=… behind a slow mirror
cargo build --release -p scion-cli
python3 bench/starhub/compare.py                  # summary in bench/starhub/compare-results.json
```

Notes:

- The Gitaly hooks reach the host at `192.168.65.254:28090`, the Docker Desktop host address. Change it in `gitaly.config.toml` elsewhere.
- Behind an HTTP proxy, set `NO_PROXY=127.0.0.1,localhost`.
- Docker Hub images come through `docker.m.daocloud.io`, and starhub's images through OpenCSG's registry.
