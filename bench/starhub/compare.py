#!/usr/bin/env python3
"""Side-by-side: starhub's git stack (Gitaly v16.4.6 + MinIO) vs Scion (PostgreSQL + MinIO), with
libgit2 on a bare copy of the same repository as the lower bound for git (no server, no network)."""
import json, os, random, shutil, statistics, subprocess, threading, time, zipfile
from pathlib import Path

C = Path(__file__).resolve().parent
SCN = C.parents[1] / "target/release/scn"
G = Path(os.environ.get("STARHUB_DIR", C.parents[2] / "starhub-server"))
ENV = dict(os.environ, NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost",
           SCION_ENDPOINT="http://127.0.0.1:17080", SCION_REPO="starhub", SCION_CONFIG_FILE=str(C / "cfg.json"))
GITURL = "http://127.0.0.1:18081/starhub.git"
# The refs imported on both sides (main, two recent branches and a tag at the time of the report)
BUNDLE_REFS = os.environ.get("BUNDLE_REFS", "main feat/lite-license-checkout mr-3172 v2.7.1-ee-alpha.1").split()
W = C / "work"
out = {"ops": {}}


def run(cmd, cwd=None, env=None, check=False):
    t = time.time()
    r = subprocess.run(cmd, cwd=cwd, env=env or ENV, capture_output=True, shell=isinstance(cmd, str))
    ms = (time.time() - t) * 1000
    if check and r.returncode:
        raise SystemExit(f"{cmd}: {r.stderr.decode()[-400:]}")
    return r, ms


def rec(section, name, **kv):
    out["ops"].setdefault(section, {})[name] = kv
    print(f"  {section:10} {name:42} {kv}", flush=True)


def dc(*a):
    return run(["docker", "compose", "-f", str(C / "docker-compose.yml"), *a])[0].stdout.decode()


def wait_ready(probe, timeout=60):
    t = time.time()
    while time.time() - t < timeout:
        if probe():
            return (time.time() - t) * 1000
        time.sleep(0.1)
    return None


# ------------------------------------------------------------------ deployment
def deployment():
    print("=== deployment", flush=True)
    for img, what in [("opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsg_public/gitaly:v16.4.6", "gitaly"),
                      ("scion:compare", "scion"),
                      ("docker.m.daocloud.io/library/postgres:16", "postgres"),
                      ("opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsg_public/bitnami/minio:2023", "minio")]:
        size = run(["docker", "image", "inspect", img, "--format", "{{.Size}}"])[0].stdout.decode().strip()
        rec("deploy", f"image {what}", mb=round(int(size or 0) / 1e6))
    gitaly_ok = lambda: run(["git", "ls-remote", GITURL])[0].returncode == 0
    scion_ok = lambda: run([str(SCN), "doctor"])[0].returncode == 0
    dc("restart", "gitaly")
    rec("deploy", "gitaly restart → serving", ms=round(wait_ready(gitaly_ok) or -1))
    dc("restart", "scion")
    rec("deploy", "scion restart → serving", ms=round(wait_ready(scion_ok) or -1))
    time.sleep(5)
    rec("deploy", "idle memory", **stats_once())


def stats_once():
    raw = run(["docker", "stats", "--no-stream", "--format", "{{.Name}} {{.CPUPerc}} {{.MemUsage}}"])[0].stdout.decode()
    res = {}
    for l in raw.strip().split("\n"):
        if not l.startswith("scioncmp-"):
            continue
        name, cpu, mem = l.split(" ", 2)
        svc = name.split("-")[1]
        res[svc] = mem.split(" / ")[0]
    return res


def to_mib(s):
    n = float("".join(ch for ch in s if ch.isdigit() or ch == "."))
    return n / 1024 if "KiB" in s else n * 1024 if "GiB" in s else n if "MiB" in s else n / 1e6


class Sampler(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.peak, self.cpu, self.stop = {}, {}, False

    def run(self):
        while not self.stop:
            raw = run(["docker", "stats", "--no-stream", "--format", "{{.Name}} {{.CPUPerc}} {{.MemUsage}}"])[0].stdout.decode()
            for l in raw.strip().split("\n"):
                if l.startswith("scioncmp-"):
                    name, cpu, mem = l.split(" ", 2)
                    svc = name.split("-")[1]
                    self.peak[svc] = max(self.peak.get(svc, 0), to_mib(mem.split(" / ")[0]))
                    self.cpu.setdefault(svc, []).append(float(cpu.rstrip("%")))


# ------------------------------------------------------------------ storage
def storage(when="before"):
    print("=== storage", when, flush=True)
    g = dc("exec", "-T", "gitaly", "du", "-sb", "/home/git/repositories/models_opencsg/starhub.git").split()[0]
    files = dc("exec", "-T", "gitaly", "sh", "-c", "find /home/git/repositories/models_opencsg/starhub.git -type f | wc -l").strip()
    rec("storage", f"gitaly repository ({when})", mb=round(int(g) / 1e6, 1), files=int(files))
    m = dc("exec", "-T", "minio", "sh", "-c", "du -sb /bitnami/minio/data/scion-objects | cut -f1; find /bitnami/minio/data/scion-objects -type f | wc -l").split()
    pg = dc("exec", "-T", "postgres", "psql", "-U", "scion", "-tAc", "select pg_database_size('scion')").strip()
    logical = dc("exec", "-T", "minio", "sh", "-c", "mc alias set loc http://127.0.0.1:9000 admin Password_123 >/dev/null && mc du --json loc/scion-objects")
    lj = json.loads(logical.strip().split("\n")[-1])
    rec("storage", f"scion objects in MinIO ({when})", mb=round(lj["size"] / 1e6, 1), objects=lj["objects"],
        minio_disk_mb=round(int(m[0]) / 1e6, 1), minio_files=int(m[1]))
    rec("storage", f"scion metadata in PostgreSQL ({when})", mb=round(int(pg) / 1e6, 1))


# ------------------------------------------------------------------ clones
def clones():
    print("=== checkout", flush=True)
    shutil.rmtree(W, ignore_errors=True)
    W.mkdir()
    cases = [
        ("git clone (full history)", ["git", "clone", "-q", GITURL, "g-full"]),
        ("git clone --depth 1", ["git", "clone", "-q", "--depth", "1", GITURL, "g-shallow"]),
        ("git clone --filter=blob:none", ["git", "clone", "-q", "--filter=blob:none", GITURL, "g-partial"]),
        ("git sparse (blob:none + sparse-checkout 2 dirs)", "git clone -q --filter=blob:none --sparse " + GITURL + " g-sparse && cd g-sparse && git sparse-checkout set common builder/store"),
        ("scn clone (direct)", [str(SCN), "clone", "starhub", "s-full"]),
        ("scn clone --sparse 2 dirs", [str(SCN), "clone", "starhub", "s-sparse", "--sparse", "common,builder/store"]),
    ]
    env_direct = dict(ENV, SCION_NO_DAEMON="1")
    for name, cmd in cases:
        r, ms = run(cmd, cwd=W, env=env_direct)
        rec("checkout", name, ms=round(ms), ok=r.returncode == 0, err=r.stderr.decode()[-120:] if r.returncode else "")
    for d in ["g-full", "s-full", "g-sparse", "s-sparse", "g-partial", "g-shallow"]:
        p = W / d
        files = sum(1 for x in p.rglob("*") if x.is_file() and ".git" not in x.parts and ".scion" not in x.parts)
        meta = sum(x.stat().st_size for x in p.rglob("*") if x.is_file() and (".git" in x.parts or ".scion" in x.parts))
        rec("checkout", f"{d} on disk", files=files, metadata_mb=round(meta / 1e6, 1))
    # status / diff speed in a clean tree
    for name, cmd, cwd in [("git status", ["git", "status", "--porcelain"], W / "g-full"),
                           ("scn status", [str(SCN), "status"], W / "s-full")]:
        ms = statistics.median([run(cmd, cwd=cwd)[1] for _ in range(5)])
        rec("local", name, ms=round(ms))
    # 10 clones at once
    for name, mk in [("10 parallel git clone --depth 1", lambda i: ["git", "clone", "-q", "--depth", "1", GITURL, f"pg{i}"]),
                     ("10 parallel scn clone", lambda i: [str(SCN), "clone", "starhub", f"ps{i}"])]:
        t = time.time()
        procs = [subprocess.Popen(mk(i), cwd=W, env=env_direct, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for i in range(10)]
        codes = [p.wait() for p in procs]
        rec("checkout", name, ms=round((time.time() - t) * 1000), failures=sum(1 for c in codes if c))


# ------------------------------------------------------------------ harness runs
def cold(service, probe):
    """Restart a server before its workload, so that no system starts with objects in memory
    (the checkouts before read everything on main)."""
    dc("restart", service)
    wait_ready(probe)
    time.sleep(3)


def harnesses():
    print("=== API workload (gitaly)", flush=True)
    cold("gitaly", lambda: run(["git", "ls-remote", GITURL])[0].returncode == 0)
    s = Sampler()
    s.start()
    r, ms = run([str(C / "gitalybench/gitalybench"), "run", str(C / "plan.json"), str(C / "gitaly-results.json")])
    s.stop = True
    s.join()
    rec("api", "gitalybench total", ms=round(ms), ok=r.returncode == 0, err=r.stderr.decode()[-200:] if r.returncode else "")
    out["peak_gitaly"] = {"mem_mib": {k: round(v) for k, v in s.peak.items()}, "cpu_max": {k: max(v) for k, v in s.cpu.items()}}
    print("=== API workload (libgit2, in process)", flush=True)
    s = Sampler()
    s.start()
    script = ("rm -rf /tmp/r && git clone -q --mirror /bench/starhub.bundle /tmp/r && "
              "gitbench /tmp/r /bench/plan.json /bench/git-results.json")
    r, ms = run(["docker", "run", "--rm", "--name", "scioncmp-gitbench-1", "-v", f"{C}:/bench",
                 "--entrypoint", "sh", "gitbench", "-c", script])
    s.stop = True
    s.join()
    rec("api", "gitbench total", ms=round(ms), ok=r.returncode == 0, err=r.stderr.decode()[-200:] if r.returncode else "")
    out["peak_git"] = {"mem_mib": {k: round(v) for k, v in s.peak.items()}, "cpu_max": {k: max(v) for k, v in s.cpu.items()}}
    print("=== API workload (scion)", flush=True)
    cold("scion", lambda: run([str(SCN), "doctor"])[0].returncode == 0)
    s = Sampler()
    s.start()
    r, ms = run([str(C / "scionbench/target/release/scionbench"), str(C / "plan.json"), str(C / "scion-results.json")])
    s.stop = True
    s.join()
    rec("api", "scionbench total", ms=round(ms), ok=r.returncode == 0, err=r.stderr.decode()[-200:] if r.returncode else "")
    out["peak_scion"] = {"mem_mib": {k: round(v) for k, v in s.peak.items()}, "cpu_max": {k: max(v) for k, v in s.cpu.items()}}


# ------------------------------------------------------------------ push
def pushes():
    print("=== push / land", flush=True)
    for n in (1, 50, 500):
        g, s = W / "g-full", W / "s-full"
        run(["git", "pull", "-q", "--rebase"], cwd=g)
        run([str(SCN), "sync"], cwd=s)
        files = sorted(p for p in (x.strip() for x in run(["git", "ls-files", "*.go"], cwd=g)[0].stdout.decode().split("\n")) if p)
        pick = random.Random(n).sample(files, n)
        for root in (g, s):
            for f in pick:
                with open(root / f, "a") as h:
                    h.write(f"// push test {n}\n")
        _, gc = run(["git", "-c", "user.name=t", "-c", "user.email=t@x", "commit", "-qam", f"push {n}"], cwd=g)
        r, gp = run(["git", "push", "-q", "origin", "main"], cwd=g)
        rec("push", f"git commit + push, {n} files", ms=round(gc + gp), commit_ms=round(gc), push_ms=round(gp), ok=r.returncode == 0,
            err=r.stderr.decode()[-150:] if r.returncode else "")
        _, sc = run([str(SCN), "commit", "-m", f"land {n}", "--author", "t <t@x>"], cwd=s)
        r, sl = run([str(SCN), "land"], cwd=s)
        rec("push", f"scn commit + land, {n} files", ms=round(sc + sl), commit_ms=round(sc), land_ms=round(sl), ok=r.returncode == 0,
            err=r.stderr.decode()[-150:] if r.returncode else "")


# ------------------------------------------------------------------ correctness
def correctness():
    print("=== correctness", flush=True)
    res = {name: json.load(open(C / f"{name}-results.json"))["values"] for name in ("gitaly", "scion", "git")}
    ops = ["read_file", "list_dir", "last_commits", "log_50", "path_log_50", "show_commit", "diff_names", "blame"]
    # (label, side a, op on a, side b, op on b); the raw-RPC variants answer the same questions
    checks = [(f"{a} vs {b}: {op}", a, op, b, op) for a, b in [("gitaly", "scion"), ("git", "gitaly"), ("git", "scion")] for op in ops]
    checks += [(f"git vs gitaly raw rpc: {op}", "git", op, "gitaly", f"{op}_raw") for op in ("list_dir", "show_commit")]
    for label, a, aop, b, bop in checks:
        av, bv = res[a].get(aop, {}), res[b].get(bop, {})
        keys = sorted(set(av) | set(bv))
        diff = [k for k in keys if av.get(k) != bv.get(k)]
        rec("correct", label, compared=len(keys), equal=len(keys) - len(diff), different=len(diff),
            example=diff[0][:60] if diff else "")
        out.setdefault("diffs", {})[label] = {k: {a: av.get(k), b: bv.get(k)} for k in diff[:5]}
    # archives: same files and contents (both zip, prefix starhub/)
    for rev in json.load(open(C / "plan.json"))["archives"]:
        def content(p):
            with zipfile.ZipFile(p) as z:
                return {n: z.read(n) for n in z.namelist() if not n.endswith("/")}
        try:
            a, b = content(f"{C}/gitaly-results.json.gitaly-{rev}.zip"), content(f"{C}/scion-results.json.scion-{rev}.zip")
            g = content(f"{C}/git-results.json.git-{rev}.zip")
            rec("correct", f"archive {rev[:8]} (git vs gitaly)", same_files=set(g) == set(a), same_content=g == a)
            rec("correct", f"archive {rev[:8]}", files=len(a), same_files=set(a) == set(b), same_content=a == b,
                only_gitaly=len(set(a) - set(b)), only_scion=len(set(b) - set(a)))
        except Exception as e:  # noqa: BLE001
            rec("correct", f"archive {rev[:8]}", error=str(e)[:100])


def reset():
    print("=== reset and import", flush=True)
    subprocess.run(["pkill", "-f", "gitalybench serve"])
    dc("down", "-v")
    dc("up", "-d", "minio", "gitaly", "postgres")
    time.sleep(8)
    dc("exec", "-T", "gitaly", "sh", "-c", "echo 0123456789abcdef0123456789abcdef > /srv/gitlab-shell/.gitlab_shell_secret")
    dc("restart", "gitaly")
    time.sleep(6)
    subprocess.Popen([str(C / "gitalybench/gitalybench"), "serve"], stdout=open(C / "serve.log", "w"), stderr=subprocess.STDOUT)
    if not (C / "starhub.bundle").exists():
        run(["git", "-C", str(G), "bundle", "create", str(C / "starhub.bundle"), *BUNDLE_REFS], check=True)
    if not (C / "plan.json").exists():
        run(["python3", str(C / "makeplan.py"), str(G), str(C / "plan.json")], check=True)
    r, ms = run([str(C / "gitalybench/gitalybench"), "import", str(C / "starhub.bundle")])
    rec("import", "gitaly CreateRepositoryFromBundle (4 refs)", ms=round(ms), ok=r.returncode == 0)
    dc("up", "-d", "scion")
    wait_ready(lambda: run([str(SCN), "doctor"])[0].returncode == 0)
    run([str(SCN), "repo", "create", "starhub"])
    total = 0
    pairs = [(r, r if r == "main" else (f"tags/{r}" if r.startswith("v") else f"imported/{r}")) for r in BUNDLE_REFS]
    for gr, sr in pairs:
        r, ms = run([str(SCN), "import", "git", str(G), "--repo", "starhub", "--git-ref", gr, "--ref", sr])
        total += ms
        rec("import", f"scion import {gr}", ms=round(ms), ok=r.returncode == 0, out=r.stdout.decode().strip()[-80:])
    rec("import", "scion import total (4 refs)", ms=round(total))
    # a bare git push of the same history into an empty Gitaly repository is what a migration would do
    rec("import", "bundle size", mb=round((C / "starhub.bundle").stat().st_size / 1e6, 1))


# ------------------------------------------------------------------ latency summary
def latency():
    print("=== latency p50 / p95 / p99 (ms)", flush=True)
    res = {name: json.load(open(C / f"{name}-results.json")) for name in ("git", "gitaly", "scion")}
    ops = sorted(set().union(*(r["timings"] for r in res.values())))

    def pct(v, q):
        v = sorted(v)
        return round(v[min(len(v) - 1, int(len(v) * q))], 1)

    for op in ops:
        row = {}
        for name, r in res.items():
            v = r["timings"].get(op)
            if v:
                row[name] = f"{pct(v, .5)} / {pct(v, .95)} / {pct(v, .99)}"
                errs = len(r.get("errors", {}).get(op, []) or [])
                if errs:
                    row[name] += f" ({errs} failed)"
        rec("latency", op, **row)
    for name, r in res.items():
        for op, v in r["values"].items():
            if op.startswith("read_concurrency_") or op.startswith("concurrent_commit"):
                rec("values", f"{name} {op}", **v)


def main():
    reset()
    deployment()
    storage()
    clones()
    harnesses()
    pushes()
    correctness()
    latency()
    storage("after the write workload")
    json.dump(out, open(C / "compare-results.json", "w"), indent=1)
    print("written", C / "compare-results.json")


if __name__ == "__main__":
    main()
