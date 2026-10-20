# CarterT27/scion's benchmarks, on this Scion

Scripts behind [docs/reports/2026-10-20-openscion-workloads.md](../../docs/reports/2026-10-20-openscion-workloads.md). They run the same workloads as [CarterT27/scion](https://github.com/CarterT27/scion)'s README, so the two implementations and Git can be compared on one machine.

| File | What |
|---|---|
| `src/main.rs` | The four synthetic workloads of [OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md), timed as CarterT27's `crates/scion-conformance/examples/benchmark.rs` times them: import a directory, commit one changed file, full checkout, one-directory checkout, against Git. In-process server, objects in memory with compression and packs |
| `t3code.py` | CarterT27's real-repository benchmark (`scripts/benchmark-git.py`) on pingdotgg/t3code at `bf8ffe81a63c`: import, storage, full and sparse checkouts checked file by file, status, history and a file read. Uses the server, PostgreSQL and MinIO of [`bench/starhub`](../starhub/README.md) |

```sh
# Synthetic: 1,000 files (7 runs) and 10,000 files (5 runs)
(cd bench/openscion && cargo build --release)
bench/openscion/target/release/openscion-bench 100 7
bench/openscion/target/release/openscion-bench 1000 5
# A third argument `plain` keeps objects in memory as they are, without compression or packs

# CarterT27's own runner, for the same machine (needs protoc: set PROTOC if it is not on PATH)
git clone https://github.com/CarterT27/scion carter && cd carter
cargo run --release -p scion-conformance --example benchmark -- 100 7

# t3code (the repository's HEAD must be the snapshot)
git clone --bare https://github.com/pingdotgg/t3code.git /tmp/t3code.git
git -C /tmp/t3code.git update-ref refs/heads/main bf8ffe81a63c82d3de5fbaa9991fd5f30034a452
git -C /tmp/t3code.git symbolic-ref HEAD refs/heads/main
docker build -t scion:compare .        # see bench/starhub/README.md for proxies
cargo build --release -p scion-cli
python3 bench/openscion/t3code.py --repo /tmp/t3code.git   # results in bench/openscion/t3code-results.json
```

`t3code.py` resets the `bench/starhub` containers (`docker compose down -v`). `--skip-import` reuses what is there.
