#!/usr/bin/env python3
"""CarterT27/scion's real-repository benchmark (scripts/benchmark-git.py) on Scion and Git.

pingdotgg/t3code at bf8ffe81a63c, the snapshot CarterT27 measured: 5,573 commits, 28,068 files.
Scion runs as in bench/starhub (server, PostgreSQL and MinIO in Docker), Git from a local bare
repository. As in the original:

- full checkout: `git worktree add` against `scn clone`;
- sparse checkout of apps/web: Git's sparse file is written outside the timer and `git reset --hard`
  is timed; `scn clone --sparse apps/web` (which also checks out the root's files);
- every checkout is checked file by file (SHA-1 and executable bit) outside the timer;
- a first checkout of each kind, then `--runs` more in random order;
- clean status, 100 first-parent history entries, reading README.md, status with one edited file:
  two warmups, then `--runs` samples each.

Usage: python3 bench/openscion/t3code.py --repo /path/to/t3code.git [--runs 5]
The repository's HEAD must be the snapshot. Results go to bench/openscion/t3code-results.json.
"""
import argparse
import hashlib
import json
import os
import random
import shutil
import statistics
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
C = ROOT / "bench" / "starhub"
SCN = ROOT / "target" / "release" / "scn"
SCOPE = "apps/web"
ENV = dict(os.environ, NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost",
           SCION_ENDPOINT="http://127.0.0.1:17080", SCION_REPO="t3code", SCION_NO_DAEMON="1",
           SCION_CONFIG_FILE=str(Path(__file__).parent / "cfg.json"))
out = {"samples": {}, "first": {}, "storage": {}, "import": {}}


def run(cmd, cwd=None, check=True):
    t = time.time()
    r = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True)
    s = time.time() - t
    if check and r.returncode:
        raise SystemExit(f"{cmd}: {r.stderr.decode()[-400:]}")
    return s, r.stdout


def dc(*a):
    return run(["docker", "compose", "-f", str(C / "docker-compose.yml"), *a])[1].decode()


def wait_ready():
    for _ in range(120):
        if subprocess.run([str(SCN), "doctor"], env=ENV, capture_output=True).returncode == 0:
            return
        time.sleep(0.5)
    raise SystemExit("scion server not ready")


def storage(when, repo):
    pack = sum(f.stat().st_size for f in (repo / "objects" / "pack").iterdir())
    lj = json.loads(dc("exec", "-T", "minio", "sh", "-c",
                       "mc alias set loc http://127.0.0.1:9000 admin Password_123 >/dev/null && "
                       "mc du --json loc/scion-objects").strip().split("\n")[-1])
    pg = int(dc("exec", "-T", "postgres", "psql", "-U", "scion", "-tAc", "select pg_database_size('scion')").strip())
    out["storage"][when] = {"git_pack_mb": round(pack / 1e6, 1), "minio_mb": round(lj["size"] / 1e6, 1),
                            "minio_objects": lj["objects"], "postgres_mb": round(pg / 1e6, 1)}
    print("storage", when, out["storage"][when], flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--repo", type=Path, required=True)
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--work", type=Path, default=Path("/tmp/scion-t3code"))
    p.add_argument("--skip-import", action="store_true")
    a = p.parse_args()
    repo, work = a.repo.resolve(), a.work
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    # Worktrees left registered by an interrupted run
    run(["git", "--git-dir", repo, "worktree", "prune"])
    sha = run(["git", "--git-dir", repo, "rev-parse", "HEAD"])[1].decode().strip()
    expected = {}
    for entry in filter(None, run(["git", "--git-dir", repo, "ls-tree", "-rz", sha])[1].split(b"\0")):
        info, path = entry.split(b"\t", 1)
        mode, kind, oid = info.split()
        if kind == b"blob":
            expected[os.fsdecode(path)] = (mode, oid)
    out["snapshot"], out["files"] = sha, len(expected)

    if not a.skip_import:
        dc("down", "-v")
        dc("up", "-d", "minio", "postgres")
        time.sleep(8)
        dc("up", "-d", "scion")
        wait_ready()
        run([str(SCN), "repo", "create", "t3code"])
        s, o = run([str(SCN), "import", "git", str(repo), "--repo", "t3code", "--git-ref", "main", "--ref", "main"])
        out["import"]["scn import git"] = {"seconds": round(s, 1), "out": o.decode().strip()[-160:]}
        print("import", out["import"], flush=True)
        storage("after import", repo)
        s, o = run(["docker", "compose", "-f", str(C / "docker-compose.yml"), "run", "--rm", "-T", "scion",
                    "admin", "repack", "--prune-after", "0s"])
        out["import"]["scion admin repack"] = {"seconds": round(s, 1), "out": o.decode().strip()[-300:]}
        print("repack", out["import"]["scion admin repack"], flush=True)
        storage("after repack", repo)
    dc("restart", "scion")
    wait_ready()

    def checkout(mode, root, sparse):
        if mode == "scion":
            cmd = [str(SCN), "clone", "t3code", str(root)] + (["--sparse", SCOPE] if sparse else [])
            return run(cmd, cwd=work)[0]
        if not sparse:
            return run(["git", "--git-dir", repo, "worktree", "add", "--detach", str(root), sha])[0]
        run(["git", "--git-dir", repo, "worktree", "add", "--detach", "--no-checkout", str(root), sha])
        loc = Path(run(["git", "-C", root, "rev-parse", "--git-path", "info/sparse-checkout"])[1].decode().strip())
        loc = loc if loc.is_absolute() else root / loc
        loc.parent.mkdir(parents=True, exist_ok=True)
        loc.write_text(f"/{SCOPE}/\n")
        return run(["git", "-C", root, "-c", "core.sparseCheckout=true", "-c", "core.sparseCheckoutCone=false",
                    "reset", "-q", "--hard", sha])[0]

    def check(root, sparse):
        actual = {str(f.relative_to(root)) for f in root.rglob("*")
                  if not ({".git", ".scion"} & set(f.relative_to(root).parts)) and (f.is_file() or f.is_symlink())}
        top = {path for path in expected if "/" not in path}
        want = {path for path in expected if not sparse or path.startswith(SCOPE + "/")}
        extra = actual - want
        # Scion's sparse checkout also has the files at the root
        assert actual >= want and (not extra or (sparse and extra <= top)), (root, list(want - actual)[:5], list(extra)[:5])
        for name in actual:
            path = root / name
            mode, oid = expected[name]
            assert path.is_symlink() == (mode == b"120000"), name
            content = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
            assert hashlib.sha1(b"blob %d\0" % len(content) + content).hexdigest().encode() == oid, name
            if not path.is_symlink():
                assert bool(path.stat().st_mode & 0o100) == (mode == b"100755"), name
        return len(actual)

    roots = {}
    worktrees = []
    for sparse in (False, True):
        name = "sparse checkout" if sparse else "full checkout"
        values = {"git": [], "scion": []}
        for trial in range(a.runs + 1):
            order = ["git", "scion"]
            random.shuffle(order)
            for mode in order:
                root = work / f"{mode}-{'sparse' if sparse else 'full'}-{trial}"
                s = checkout(mode, root, sparse)
                if mode == "git":
                    worktrees.append(root)
                n = check(root, sparse)
                if trial == 0:
                    out["first"].setdefault(name, {})[mode] = round(s, 3)
                else:
                    values[mode].append(s)
                roots[(mode, sparse)] = root
                out.setdefault("files_checked_out", {})[f"{mode} {name}"] = n
                print(name, trial, mode, round(s, 3), "s", n, "files", flush=True)
        out["samples"][name] = values

    def measure(name, commands):
        for _ in range(2):
            for mode, (cmd, cwd) in commands.items():
                run(cmd, cwd=cwd)
        values = {mode: [] for mode in commands}
        for _ in range(a.runs + 2):
            order = list(commands)
            random.shuffle(order)
            for mode in order:
                cmd, cwd = commands[mode]
                values[mode].append(run(cmd, cwd=cwd)[0])
        out["samples"][name] = values
        print(name, {m: round(statistics.median(v), 4) for m, v in values.items()}, flush=True)

    for sparse in (False, True):
        g, s = roots[("git", sparse)], roots[("scion", sparse)]
        label = "sparse " if sparse else ""
        gitc = ["git", "-c", f"core.sparseCheckout={'true' if sparse else 'false'}", "-c", "core.sparseCheckoutCone=false"]
        status = {"git": (gitc + ["status", "--porcelain=v1", "--untracked-files=all"], g), "scion": ([str(SCN), "status"], s)}
        measure(label + "clean status", status)
        if not sparse:
            measure("history (100 first-parent commits)", {
                "git": (["git", "log", "--first-parent", "-n", "100", "--format=%h %s", sha], g),
                "scion": ([str(SCN), "log", "-n", "100"], s)})
            want = run(["git", "--git-dir", repo, "show", f"{sha}:README.md"])[1]
            assert run([str(SCN), "cat", "main:README.md"], cwd=s)[1] == want
            measure("read README.md", {"git": (["git", "show", f"{sha}:README.md"], g),
                                       "scion": ([str(SCN), "cat", "main:README.md"], s)})
        edited = next(p for p, (m, _) in sorted(expected.items()) if (not sparse or p.startswith(SCOPE + "/")) and m == b"100644")
        originals = {r: (r / edited).read_bytes() for r in (g, s)}
        try:
            for r, o in originals.items():
                (r / edited).write_bytes(o + b"\nTemporary benchmark edit.\n")
            measure(label + "status (one edited file)", status)
        finally:
            for r, o in originals.items():
                (r / edited).write_bytes(o)

    out["summary"] = {name: {m: round(statistics.median(v), 3) for m, v in vs.items()} for name, vs in out["samples"].items()}
    json.dump(out, open(Path(__file__).parent / "t3code-results.json", "w"), indent=1)
    print(json.dumps(out["summary"], indent=1))
    for root in worktrees:
        subprocess.run(["git", "--git-dir", repo, "worktree", "remove", "--force", root], capture_output=True)
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
