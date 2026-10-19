#!/usr/bin/env python3
"""Full real-world test of scion on an import of starhub-server, checked against git.

STARHUB_DIR (default ../../../starhub-server next to this repository) is the git repository to
import; scion and scn are taken from this repository's target/release. Results go to ./full/."""
import hashlib, json, os, random, shutil, subprocess, sys, tarfile, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
S = HERE / "full"
B = HERE.parents[1] / "target/release"
G = Path(os.environ.get("STARHUB_DIR", HERE.parents[2] / "starhub-server"))
PORT = 17095
ENV = dict(os.environ, NO_PROXY="127.0.0.1,localhost", SCION_ENDPOINT=f"http://127.0.0.1:{PORT}",
           SCION_CONFIG_FILE=str(S / "cfg.json"), SCION_AUTHOR="")
ENV.pop("SCION_AUTHOR")
random.seed(20261019)
results = []   # (section, check, ok, detail, ms)
timings = []   # (section, op, ms)
section = "setup"


def sec(name):
    global section
    section = name
    print(f"\n=== {name}", flush=True)


def check(name, ok, detail="", ms=None):
    results.append((section, name, bool(ok), str(detail), ms))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}{' — ' + str(detail) if detail else ''}{f' ({ms:.0f} ms)' if ms is not None else ''}", flush=True)
    return ok


def timed(op, ms):
    timings.append((section, op, ms))


def sh(cmd, cwd=None, inp=None):
    return subprocess.run(cmd, cwd=cwd, input=inp, capture_output=True, env=ENV)


def git(*args, raw=False):
    r = subprocess.run(["git", "-C", str(G), *args], capture_output=True)
    return r.stdout if raw else r.stdout.decode(errors="replace")


def scn(*args, cwd=None, js=False, op=None):
    t = time.time()
    args = list(args)
    if js:
        args.insert(args.index("--") if "--" in args else len(args), "--json")
    r = subprocess.run([str(B / "scn"), *args], cwd=cwd, capture_output=True, env=ENV)
    ms = (time.time() - t) * 1000
    if op:
        timed(op, ms)
    out = r.stdout
    if js and r.returncode == 0:
        try:
            out = json.loads(r.stdout)
        except json.JSONDecodeError:
            pass
    return r.returncode, out, r.stderr.decode(errors="replace"), ms


def tree_of(root, skip=(".scion",)):
    """path -> (kind, sha256 or link target, executable)"""
    out = {}
    root = Path(root)
    for dirpath, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if not (Path(dirpath) == root and d in skip)]
        for f in files + [d for d in dirs if os.path.islink(os.path.join(dirpath, d))]:
            p = Path(dirpath) / f
            rel = str(p.relative_to(root))
            if p.is_symlink():
                out[rel] = ("link", os.readlink(p), False)
            elif p.is_file():
                out[rel] = ("file", hashlib.sha256(p.read_bytes()).hexdigest(), os.access(p, os.X_OK))
    return out


def git_tree(rev, sub=None):
    d = S / f"git-{rev.replace('/', '_').replace('~', '-').replace('^', '-')}"
    if not d.exists():
        d.mkdir(parents=True)
        data = git("archive", rev, *( [sub] if sub else []), raw=True)
        with tarfile.open(fileobj=__import__("io").BytesIO(data)) as t:
            t.extractall(d, filter="tar")
    return tree_of(d)


def first_parent_changes():
    """commit -> set of paths changed relative to its first parent (root: all files)."""
    lines = git("rev-list", "--parents", "main").split("\n")
    pairs = []
    for l in lines:
        ids = l.split()
        if ids:
            pairs.append((ids[0], ids[1] if len(ids) > 1 else None))
    stdin = "".join(f"{c} {p}\n" for c, p in pairs if p)
    out = subprocess.run(["git", "-C", str(G), "diff-tree", "--stdin", "-r", "--name-only", "--no-renames"],
                         input=stdin.encode(), capture_output=True).stdout.decode(errors="replace")
    changes, cur = {}, None
    for l in out.split("\n"):
        if len(l) == 40 and all(ch in "0123456789abcdef" for ch in l):
            cur = l
            changes[cur] = set()
        elif l and cur:
            changes[cur].add(l)
    for c, p in pairs:
        if p is None:
            changes[c] = set(git("ls-tree", "-r", "--name-only", c).split("\n")) - {""}
        changes.setdefault(c, set())
    return changes, dict(pairs)


def log_ids(rev="main", path=None, n=100000, cwd=None):
    args = ["log", rev, "-n", str(n)] + (["--", path] if path else [])
    rc, out, err, ms = scn(*args, cwd=cwd, js=True, op=f"log {'-- path' if path else ''} -n {n}")
    return (rc, [e["id"] for e in out["entries"]] if rc == 0 else err, ms)


def git_meta(sha):
    """(author time seconds, subject) for matching scion and git commits."""
    t, b = git("log", "-1", "--format=%at%x00%B", sha).split("\x00", 1)
    return int(t), b.strip("\n").split("\n", 1)[0]


def scn_meta(rev, cwd):
    rc, out, err, _ = scn("log", rev, "-n", "1", cwd=cwd, js=True)
    if rc != 0:
        return None
    e = out["entries"][0]
    return e["time_ms"] // 1000, e["message"].strip("\n").split("\n", 1)[0]


# ------------------------------------------------------------------ setup
def setup():
    sec("1. Setup and import")
    subprocess.run(["pkill", "-f", f"listen 127.0.0.1:{PORT}"])
    time.sleep(1)
    if S.exists():
        shutil.rmtree(S)
    S.mkdir(parents=True)
    log = open(S / "server.txt", "w")
    subprocess.Popen([str(B / "scion"), "server", "--lite", "--data-dir", str(S / "data"), "--listen", f"127.0.0.1:{PORT}"],
                     stdout=log, stderr=subprocess.STDOUT, env=ENV)
    for _ in range(50):
        if scn("doctor")[0] == 0:
            break
        time.sleep(0.2)
    check("server starts on an empty data directory", scn("doctor")[0] == 0)
    scn("config", "set", "user.name", "Tester")
    scn("config", "set", "user.email", "tester@example.com")
    check("create repository", scn("repo", "create", "starhub")[0] == 0)
    n = int(git("rev-list", "--count", "main"))
    rc, out, err, ms = scn("import", "git", str(G), "--repo", "starhub", "--git-ref", "main", "--ref", "main", js=True, op="import main")
    check(f"import main ({n} commits)", rc == 0 and out["commits"] == n, f"{n / (ms / 1000):.0f} commits/s", ms)
    branches = [b for b in git("for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads").split() if b != "main"][:2]
    for br in branches:
        rc, out, err, ms = scn("import", "git", str(G), "--repo", "starhub", "--git-ref", br, "--ref", f"imported/{br}", js=True, op="import branch")
        check(f"import branch {br}", rc == 0, err.strip()[:120], ms)
    tag = git("describe", "--tags", "--abbrev=0", "main").strip()
    if tag:
        rc, out, err, ms = scn("import", "git", str(G), "--repo", "starhub", "--git-ref", tag, "--ref", f"tags/{tag}", js=True, op="import tag")
        check(f"import tag {tag} into tags/{tag}", rc == 0, err.strip()[:160], ms)
    return branches, tag


# ------------------------------------------------------------------ read
def read_tests(tag):
    sec("2. Checkout and reading files")
    rc, out, err, ms = scn("clone", "starhub", str(S / "wc1"), js=True, op="clone full")
    check("clone full checkout", rc == 0, f"{len(git('ls-files').split())} files", ms)
    wc1 = S / "wc1"
    t_git = git_tree("main")
    t_wc = tree_of(wc1)
    check("every file, executable bit and symlink equals git archive main", t_git == t_wc,
          f"{len(t_wc)} paths" + ("" if t_git == t_wc else f"; only git {len(set(t_git) - set(t_wc))}, only scion {len(set(t_wc) - set(t_git))}, differ {sum(1 for p in t_git if p in t_wc and t_git[p] != t_wc[p])}"))
    rc, out, err, ms = scn("clone", "starhub", str(S / "wc2"), op="clone full (warm daemon)")
    check("second full clone", rc == 0, "", ms)
    dirs = ["common", "builder/store"]
    rc, out, err, ms = scn("clone", "starhub", str(S / "sparse"), "--sparse", ",".join(dirs), op="clone sparse")
    want = {p for p in git("ls-files").split("\n") if p and ("/" not in p or any(p.startswith(d + "/") for d in dirs))}
    got = set(tree_of(S / "sparse"))
    check(f"sparse clone of {dirs} has exactly those files plus root files", rc == 0 and want == got, f"{len(got)} files", ms)

    # ls
    rc, out, err, ms = scn("ls", "main", cwd=wc1, js=True, op="ls root")
    want = sorted(set(l.split("\t")[1] for l in git("ls-tree", "main").split("\n") if l and not l.startswith("160000")))
    check("ls root equals git ls-tree", rc == 0 and sorted(e["name"] for e in out["entries"]) == want, f"{len(want)} entries", ms)
    old = git("rev-parse", "main~2000").strip()
    rc, out, err, ms = scn("ls", f"{old}:builder", cwd=wc1, js=True, op="ls old subdir")
    want = sorted(l.split("\t")[1] for l in git("ls-tree", f"{old}:builder").split("\n") if l and not l.startswith("160000"))
    check("ls builder/ at main~2000 (by git SHA) equals git", rc == 0 and sorted(e["name"] for e in out["entries"]) == want, f"{len(want)} entries", ms)

    # cat at random old revisions
    revs = git("rev-list", "main").split()
    bad, n, total = [], 0, 0
    for _ in range(40):
        c = random.choice(revs)
        files = git("ls-tree", "-r", "--name-only", c).split("\n")
        f = random.choice([x for x in files if x])
        want = git("show", f"{c}:{f}", raw=True)
        rc, out, err, ms = scn("cat", f"{c}:{f}", cwd=wc1, op="cat old revision")
        total += ms
        n += 1
        if rc != 0 or out != want:
            bad.append(f"{c[:8]}:{f}")
    check("cat 40 random files at random revisions byte-equal to git show", not bad, ", ".join(bad[:3]), total / n)

    # archive
    rc, out, err, ms = scn("archive", f"{old}:builder", "--format", "tar.gz", "-o", str(S / "old.tgz"), cwd=wc1, op="archive subdir tar.gz")
    d = S / "arch-old"
    d.mkdir()
    with tarfile.open(S / "old.tgz") as t:
        t.extractall(d, filter="tar")
    check("archive builder/ at main~2000 equals git archive", rc == 0 and tree_of(d) == git_tree(old, "builder"), "", ms)
    rc, out, err, ms = scn("archive", "main", "--format", "zip", "-o", str(S / "main.zip"), cwd=wc1, op="archive full zip")
    z = subprocess.run(["unzip", "-q", "-o", str(S / "main.zip"), "-d", str(S / "arch-zip")], capture_output=True)
    check("archive main as zip unpacks to the same tree", rc == 0 and z.returncode == 0 and tree_of(S / "arch-zip") == t_git,
          f"{(S / 'main.zip').stat().st_size / 1e6:.1f} MB", ms)
    if tag:
        rc, out, err, ms = scn("ls", tag, cwd=wc1, js=True)
        check(f"tag name {tag} works as a revision", rc == 0, err.strip()[:100], ms)
    return wc1


# ------------------------------------------------------------------ history
def history_tests(wc1):
    sec("3. History queries")
    changes, parent = first_parent_changes()
    n = int(git("rev-list", "--count", "main"))
    rc, ids, ms = log_ids(n=n + 10, cwd=wc1)
    check("full log lists every commit once", rc == 0 and len(ids) == n == len(set(ids)), f"{len(ids)} commits", ms)
    rc, out, err, ms = scn("log", "-n", "50", cwd=wc1, js=True, op="log -n 50")
    git50 = [git_meta(s) for s in git("log", "-n", "50", "--topo-order", "--format=%H", "main").split()]
    ours50 = [(e["time_ms"] // 1000, e["message"].strip("\n").split("\n")[0]) for e in out["entries"]]
    check("first 50 log entries are the same commits as git log --topo-order", set(ours50) == set(git50), "", ms)

    # path logs
    files = [p for p in git("ls-files").split("\n") if p]
    paths = random.sample(files, 10) + ["builder", "common/config", "api/handler", "docker", "go.mod"]
    bad = []
    for p in paths:
        exp = {c for c, ch in changes.items() if any(x == p or x.startswith(p + "/") for x in ch)}
        exp_meta = {git_meta(c) for c in list(exp)[:400]} if len(exp) <= 400 else None
        rc, got, ms = log_ids(path=p, cwd=wc1)
        if rc != 0:
            bad.append(f"{p}: {got[:80]}")
            continue
        if len(got) != len(exp):
            bad.append(f"{p}: scion {len(got)} vs git {len(exp)}")
            continue
        if exp_meta is not None:
            rc, out, err, _ = scn("log", "-n", str(len(got) + 1), "--", p, cwd=wc1, js=True)
            gm = {(e["time_ms"] // 1000, e["message"].strip("\n").split("\n")[0]) for e in out["entries"]}
            if gm != exp_meta:
                bad.append(f"{p}: commits differ")
    check(f"log -- PATH for {len(paths)} files and directories matches git first-parent changes", not bad, "; ".join(bad[:3]))

    # diff pairs
    revs = git("rev-list", "main").split()
    bad, tot = [], 0
    for _ in range(25):
        a, b = random.sample(revs[:3000], 2)
        want = sorted(l for l in git("diff", "--name-status", "--no-renames", a, b).split("\n") if l)
        rc, out, err, ms = scn("diff", f"{a}..{b}", "--name-only", cwd=wc1, js=True, op="diff A..B")
        tot += ms
        got = sorted(f"{f['status']}\t{f['path']}" for f in out["files"]) if rc == 0 else [err]
        if got != want:
            bad.append(f"{a[:8]}..{b[:8]}: {len(got)} vs {len(want)}")
    check("diff A..B --name-only for 25 random pairs equals git diff --name-status", not bad, "; ".join(bad[:3]), tot / 25)
    a, b = revs[400], revs[900]
    mb = git("merge-base", a, b).strip()
    want = sorted(l for l in git("diff", "--name-status", "--no-renames", mb, a).split("\n") if l)
    rc, out, err, ms = scn("diff", f"{b}...{a}", "--name-only", cwd=wc1, js=True, op="diff A...B")
    got = sorted(f"{f['status']}\t{f['path']}" for f in out["files"]) if rc == 0 else [err]
    check("diff A...B (from the merge base) equals git", got == want, f"{len(got)} files", ms)
    rc, out, err, ms = scn("diff", f"{revs[300]}..{revs[200]}", cwd=wc1, op="diff with patches")
    gitp = git("diff", "--no-renames", "--minimal", revs[300], revs[200])
    def pm(t):
        ls = t.split("\n")
        return (sum(1 for l in ls if l.startswith("+") and not l.startswith("+++")),
                sum(1 for l in ls if l.startswith("-") and not l.startswith("---")))
    ours, theirs = pm(out.decode(errors="replace")), pm(gitp)
    check("diff with patches adds and removes as many lines as git diff --minimal", rc == 0 and ours == theirs,
          f"+/- {ours} vs {theirs}", ms)

    # show
    bad, tot = [], 0
    for c in random.sample(revs, 20):
        p = parent.get(c)
        want = sorted(l for l in (git("diff", "--name-status", "--no-renames", p, c) if p else
                                  "\n".join("A\t" + x for x in changes[c])).split("\n") if l)
        rc, out, err, ms = scn("show", c, "--name-only", cwd=wc1, js=True, op="show")
        tot += ms
        got = sorted(f"{f['status']}\t{f['path']}" for f in out["diff"]["files"]) if rc == 0 else [err]
        meta_ok = rc == 0 and (out["commit"]["time_ms"] // 1000, out["commit"]["message"].strip("\n").split("\n")[0]) == git_meta(c)
        if got != want or not meta_ok:
            bad.append(c[:8])
    check("show (metadata and files) for 20 random commits, including merges, equals git", not bad, ", ".join(bad[:4]), tot / 20)

    # blame
    bad, tot = [], 0
    targets = random.sample([f for f in files if f.endswith((".go", ".md", ".yaml"))], 15)
    renamed_lines = []
    for f in targets:
        g = git("blame", "--first-parent", "--line-porcelain", "main", "--", f)
        want = []
        for l in g.split("\n"):
            if l.startswith("author "):
                who = l[7:]
            elif l.startswith("author-time "):
                when = int(l[12:])
            elif l.startswith("filename "):
                want.append((who, when, l[9:] == f))
        rc, out, err, ms = scn("blame", f, cwd=wc1, js=True, op="blame")
        tot += ms
        got = [(x["author"], x["time_ms"] // 1000) for x in out["lines"]] if rc == 0 else []
        renamed = sum(1 for w in want if not w[2])
        same = len(got) == len(want) and all(g2 == w[:2] for g2, w in zip(got, want) if w[2])
        if not same:
            diff_at = next((i for i, (x, y) in enumerate(zip(got, want)) if y[2] and x != y[:2]), -1)
            bad.append(f"{f} (line {diff_at + 1}, {len(got)} vs {len(want)} lines)")
        elif renamed:
            renamed_lines.append(f"{f}: {renamed} lines git traces through a rename or copy")
    check("blame of 15 files: every line's author and time equal git blame --first-parent "
          "(except lines git follows through renames/copies)", not bad, "; ".join(bad[:2] or renamed_lines[:2]), tot / 15)
    old = revs[1500]
    f = next(x for x in git("ls-tree", "-r", "--name-only", old).split("\n") if x.endswith(".go"))
    g = git("blame", "--first-parent", "--line-porcelain", old, "--", f)
    want = [int(l[12:]) for l in g.split("\n") if l.startswith("author-time ")]
    rc, out, err, ms = scn("blame", f"{old}:{f}", cwd=wc1, js=True, op="blame old revision")
    check("blame at an old revision equals git", rc == 0 and [x["time_ms"] // 1000 for x in out["lines"]] == want, f, ms)

    # revisions and git SHAs
    bad = []
    for c in random.sample(revs, 30):
        if scn_meta(c, wc1) != git_meta(c):
            bad.append(c[:8])
    check("30 random git SHAs resolve to the matching commits", not bad, ", ".join(bad[:4]))
    bad = []
    for r in ["main~1", "main~10", "main~500", "main^", "main^2", "main~3^2", "main^2~5", "HEAD~7", "@^"]:
        g = r.replace("HEAD", "main").replace("@", "main")
        if scn_meta(r, wc1) != git_meta(git("rev-parse", g).strip()):
            bad.append(r)
    check("relative revisions (~N, ^, ^2, combinations, HEAD, @) match git", not bad, ", ".join(bad))


# ------------------------------------------------------------------ mass writes
def mass_write_tests(wc1, wc2):
    sec("4. Large changes")
    files = sorted(p for p in tree_of(wc1) if not p.startswith(".scion"))
    go = [f for f in files if f.endswith(".go")]
    random.shuffle(go)
    mod, rm, mv = go[:1500], go[1500:1800], go[1800:1900]
    for f in mod:
        with open(wc1 / f, "a") as h:
            h.write("\n// scion full test edit\n")
    for f in rm:
        os.remove(wc1 / f)
    for f in mv:
        dst = wc1 / "moved" / f
        dst.parent.mkdir(parents=True, exist_ok=True)
        os.rename(wc1 / f, dst)
    for i in range(600):
        p = wc1 / "newpkg" / f"sub{i % 20}" / f"file{i}.go"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(f"package sub{i % 20}\n\n// generated {i}\nfunc F{i}() int {{ return {i} }}\n")
    sh_files = [f for f in files if f.endswith(".sh")][:10] or [mod[0]]
    for f in sh_files:
        if (wc1 / f).exists():
            os.chmod(wc1 / f, 0o755 if not os.access(wc1 / f, os.X_OK) else 0o644)
    for i in range(5):
        os.symlink("../go.mod", wc1 / "newpkg" / f"link{i}")
    # ignored files: starhub's .gitignore patterns
    ign = (wc1 / ".gitignore").read_text().split("\n") if (wc1 / ".gitignore").exists() else []
    ignored = ["bin/starhub-server", ".env", "local_only/notes.txt", "api/.DS_Store"]
    for f in ignored:
        (wc1 / f).parent.mkdir(parents=True, exist_ok=True)
        (wc1 / f).write_text("ignored\n")
    rc, out, err, ms = scn("status", cwd=wc1, js=True, op="status (3,000+ changes)")
    exp_added = 600 + 5 + len(mv)
    got = (len(out["added"]), len(out["modified"]), len(out["deleted"])) if rc == 0 else None
    exp_mod = len(mod) + sum(1 for f in sh_files if f not in mod and f not in rm and f not in mv)
    check("status finds every add, modify, delete, move and mode change",
          got == (exp_added, exp_mod, len(rm) + len(mv)), f"A/M/D {got} expected {(exp_added, exp_mod, len(rm) + len(mv))}", ms)
    ignored_shown = [p for p in out["added"] if p in ignored] if rc == 0 else []
    check("starhub's .gitignore (bin, .env, local_only, .DS_Store) hides those files", not ignored_shown,
          f"shown {ignored_shown}")
    rc, out, err, ms = scn("diff", "--name-only", cwd=wc1, js=True, op="diff local (3,000+ files)")
    check("local diff lists every changed file", rc == 0 and len(out["files"]) == exp_added + exp_mod + len(rm) + len(mv), f"{len(out['files']) if rc == 0 else err}", ms)
    rc, out, err, ms = scn("diff", cwd=wc1, op="diff local with patches")
    check("local diff with patches", rc == 0, f"{len(out) / 1e6:.1f} MB of diff", ms)
    rc, out, err, ms = scn("commit", "-m", "full test: 3,000+ file changes", cwd=wc1, js=True, op="commit 3,000+ changes")
    check("commit 3,000+ changes", rc == 0 and out.get("changeset"), err[:100], ms)
    rc, out, err, ms = scn("land", cwd=wc1, js=True, op="land 3,000+ changes")
    check("land", rc == 0, err[:100], ms)
    rc, out, err, ms = scn("status", cwd=wc1, js=True)
    check("clean after land (ignored files not counted)", rc == 0 and not (out["added"] or out["modified"] or out["deleted"]),
          f"{len(out['added'])} added left" if rc == 0 else err)
    rc, out, err, ms = scn("archive", "main", "--format", "tar", "-o", str(S / "after.tar"), cwd=wc1, op="archive after land")
    d = S / "arch-after"
    d.mkdir()
    with tarfile.open(S / "after.tar") as t:
        t.extractall(d, filter="tar")
    mine = {k: v for k, v in tree_of(wc1).items() if k not in ignored}
    check("server snapshot after land equals the working copy", tree_of(d) == mine, f"{len(mine)} paths", ms)
    rc, out, err, ms = scn("sync", cwd=wc2, js=True, op="sync 3,000+ changes")
    check("second working copy syncs and equals the first", rc == 0 and tree_of(wc2) == mine, err[:100], ms)
    for f in ignored:
        os.remove(wc1 / f)
    rc, out, err, ms = scn("blame", mod[0], cwd=wc1, js=True)
    check("blame attributes the appended line to the new commit", rc == 0 and out["lines"][-1]["author"] == "Tester", mod[0], ms)
    rc, out, err, ms = scn("log", "-n", "3", "--", "newpkg", cwd=wc1, js=True)
    check("log -- newpkg shows the commit that created it", rc == 0 and len(out["entries"]) == 1, "", ms)
    return mod


# ------------------------------------------------------------------ collaboration
def collab_tests(wc1, wc2, mod):
    sec("5. Two developers")
    a, b = mod[:50], mod[50:100]
    for f in a:
        p = wc1 / f
        p.write_text("// top by A\n" + p.read_text())
    for f in b:
        p = wc2 / f
        p.write_text("// top by B\n" + p.read_text())
    scn("commit", "-m", "A: 50 files", cwd=wc1)
    rc, _, err, ms = scn("land", cwd=wc1, op="land")
    scn("commit", "-m", "B: 50 other files", cwd=wc2)
    rc, out, err, ms = scn("land", cwd=wc2, js=True, op="land behind (server rebase)")
    check("land while behind: the server rebases disjoint changes", rc == 0 and out.get("rebased"), err[:100], ms)
    rc, out, err, ms = scn("sync", cwd=wc1, js=True, op="sync")
    check("both sets of changes end up in both working copies", rc == 0 and tree_of(wc1) == tree_of(wc2), "", ms)
    # same files, different lines: client-side line merge during sync
    c = mod[100:130]
    for f in c:
        p = wc1 / f
        p.write_text("// first line by A\n" + p.read_text())
        p = wc2 / f
        p.write_text(p.read_text() + "// last line by B\n")
    scn("commit", "-m", "A: tops", cwd=wc1)
    scn("land", cwd=wc1)
    scn("commit", "-m", "B: bottoms", cwd=wc2)
    rc, out, err, ms = scn("sync", cwd=wc2, js=True, op="sync with line merges (30 files)")
    merged = all((wc2 / f).read_text().startswith("// first line by A") and (wc2 / f).read_text().endswith("// last line by B\n") for f in c)
    check("sync merges different lines of the same 30 files", rc == 0 and merged, err[:120], ms)
    rc, out, err, ms = scn("land", cwd=wc2, op="land")
    scn("sync", cwd=wc1)
    check("land the line-merged commit; trees equal", rc == 0 and tree_of(wc1) == tree_of(wc2), err[:100], ms)


def conflict_tests(wc1, wc2, mod):
    sec("6. Conflicts")
    c = mod[200:225]
    for f in c:
        lines = (wc1 / f).read_text().split("\n")
        (wc1 / f).write_text("\n".join(["// A wins"] + lines[1:]))
        lines = (wc2 / f).read_text().split("\n")
        (wc2 / f).write_text("\n".join(["// B wins"] + lines[1:]))
    scn("commit", "-m", "A: first lines", cwd=wc1)
    scn("land", cwd=wc1)
    scn("commit", "-m", "B: first lines", cwd=wc2)
    rc, out, err, ms = scn("land", cwd=wc2, op="land with conflicts")
    check("land reports the conflicts and changes nothing", rc != 0 and "conflict" in err.lower(), err.strip().split("\n")[0][:120], ms)
    rc, out, err, ms = scn("sync", cwd=wc2, op="sync with 25 conflicts")
    rc2, st, _, _ = scn("status", cwd=wc2, js=True)
    check("sync stops with conflict markers in all 25 files (exit 3)", rc == 3 and len(st["conflicted"]) == 25 and
          all("<<<<<<<" in (wc2 / f).read_text() for f in c), f"exit {rc}, {len(st['conflicted'])} conflicted", ms)
    rc, *_ = scn("commit", "-m", "x", cwd=wc2)
    check("commit refuses while conflicts are unresolved", rc == 3)
    rc, *_ = scn("resolve", c[0], cwd=wc2)
    check("resolve refuses a file that still has markers", rc != 0)
    rc, *_ = scn("resolve", "--theirs", *c[:10], cwd=wc2)
    ok_t = rc == 0 and all((wc2 / f).read_text().startswith("// A wins") for f in c[:10])
    rc, *_ = scn("resolve", "--ours", *c[10:15], cwd=wc2)
    ok_o = rc == 0 and all((wc2 / f).read_text().startswith("// B wins") for f in c[10:15])
    for f in c[15:]:
        t = (wc2 / f).read_text()
        body = t.split(">>>>>>>", 1)[1].split("\n", 1)[1]
        (wc2 / f).write_text("// A and B agreed\n" + body)
    rc, out, err, _ = scn("resolve", *c[15:], cwd=wc2, js=True)
    check("resolve --theirs (10), --ours (5) and hand-edited (10)", ok_t and ok_o and rc == 0 and out["remaining"] == [], err[:100])
    rc, out, err, ms = scn("commit", cwd=wc2, js=True, op="commit after resolving")
    rc2, _, err2, _ = scn("land", cwd=wc2)
    rc3, out3, _, _ = scn("log", "-n", "1", cwd=wc2, js=True)
    check("commit without -m reuses the message; land", rc == 0 and rc2 == 0 and out3["entries"][0]["message"] == "B: first lines", err + err2, ms)
    scn("sync", cwd=wc1)
    check("working copies agree after the resolution", tree_of(wc1) == tree_of(wc2))


def branch_tests(wc1, mod, branches):
    sec("7. Branches and merges")
    rc, out, err, ms = scn("switch", "-c", "full-test-feature", cwd=wc1, js=True, op="switch -c")
    check("switch -c creates a branch at the current commit", rc == 0, err[:100], ms)
    f_only = mod[300:500]
    for f in f_only:
        with open(wc1 / f, "a") as h:
            h.write("// on feature\n")
    overlap = mod[500:505]
    for f in overlap:
        p = wc1 / f
        p.write_text("// feature top\n" + "\n".join(p.read_text().split("\n")[1:]))
    scn("commit", "-m", "feature work", cwd=wc1)
    rc, *_ = scn("land", cwd=wc1)
    check("land on the feature branch", rc == 0)
    rc, out, err, ms = scn("switch", "main", cwd=wc1, js=True, op="switch (205 files differ)")
    check("switch back to main rewrites the changed files", rc == 0 and out["written"] == 205, f"written {out.get('written') if rc == 0 else err}", ms)
    for f in mod[600:700]:
        with open(wc1 / f, "a") as h:
            h.write("// on main\n")
    for f in overlap:
        p = wc1 / f
        p.write_text("// main top\n" + "\n".join(p.read_text().split("\n")[1:]))
    scn("commit", "-m", "main work", cwd=wc1)
    scn("land", cwd=wc1)
    rc, out, err, ms = scn("merge", "full-test-feature", cwd=wc1, op="merge with 5 conflicts")
    rc2, st, _, _ = scn("status", cwd=wc1, js=True)
    check("merge stops on the 5 overlapping files", rc == 3 and sorted(st["conflicted"]) == sorted(overlap), f"exit {rc}", ms)
    rc, out, err, ms = scn("merge", "--abort", cwd=wc1, op="merge --abort")
    rc2, st, _, _ = scn("status", cwd=wc1, js=True)
    check("merge --abort restores everything", rc == 0 and not (st["added"] or st["modified"] or st["deleted"] or st["conflicted"]), err[:80], ms)
    scn("merge", "full-test-feature", cwd=wc1)
    scn("resolve", "--theirs", ".", cwd=wc1)
    rc, out, err, ms = scn("commit", cwd=wc1, js=True)
    rc2, *_ = scn("land", cwd=wc1)
    rc3, lg, _, _ = scn("log", "-n", "1", cwd=wc1, js=True)
    feat_ok = all((wc1 / f).read_text().endswith("// on feature\n") for f in f_only)
    check("resolved merge lands as a merge commit with both sides' changes",
          rc == 0 and rc2 == 0 and len(lg["entries"][0]["parents"]) == 2 and feat_ok, err[:100])
    rc, out, err, ms = scn("merge", "full-test-feature", cwd=wc1, op="merge again")
    check("merging again is a no-op", rc == 0 and b"up to date" in out, "", ms)
    if branches:
        br = f"imported/{branches[0]}"
        rc, out, err, ms = scn("merge", br, cwd=wc1, op="merge imported branch")
        if rc == 3:
            scn("resolve", "--ours", ".", cwd=wc1)
            rc, out, err, _ = scn("commit", cwd=wc1)
        rc2, _, err2, _ = scn("land", cwd=wc1)
        check(f"merge the imported git branch {branches[0]} and land", rc == 0 and rc2 == 0, (err + err2)[:120], ms)
    rc, *_ = scn("branch", "-d", "full-test-feature", cwd=wc1)
    check("delete the feature branch", rc == 0)


def undo_tests(wc1, mod):
    sec("8. Undo, amend, tags, git spellings")
    for f in mod[700:760]:
        (wc1 / f).write_text("broken\n")
    rc, out, err, ms = scn("restore", "api", cwd=wc1, js=True, op="restore dir")
    left = [f for f in mod[700:760] if (wc1 / f).read_text() == "broken\n"]
    check("restore a directory puts its files back", rc == 0 and all(not f.startswith("api/") for f in left), f"{len(out.get('restored', []))} restored", ms)
    rc, out, err, ms = scn("restore", ".", cwd=wc1, js=True, op="restore all")
    check("restore . puts everything back", rc == 0 and all((wc1 / f).read_text() != "broken\n" for f in mod[700:760]), "", ms)
    (wc1 / "amend1.txt").write_text("1\n")
    scn("commit", "-m", "to amend", cwd=wc1)
    (wc1 / "amend2.txt").write_text("2\n")
    rc, *_ = scn("commit", "--amend", "-m", "amended", cwd=wc1)
    rc2, out, _, _ = scn("uncommit", cwd=wc1, js=True)
    rc3, st, _, _ = scn("status", cwd=wc1, js=True)
    check("amend, then uncommit keeps both files as local changes", rc == 0 and rc2 == 0 and sorted(st["added"]) == ["amend1.txt", "amend2.txt"])
    os.remove(wc1 / "amend1.txt")
    os.remove(wc1 / "amend2.txt")
    rc, out, err, ms = scn("tag", "full-test-1", cwd=wc1, js=True, op="tag")
    rc2, out2, _, _ = scn("tag", cwd=wc1, js=True)
    rc3, lg, _, _ = scn("log", "full-test-1", "-n", "1", cwd=wc1, js=True)
    rc4, hd, _, _ = scn("log", "-n", "1", cwd=wc1, js=True)
    check("tag, list tags, use a tag as a revision", rc == 0 and any(t["name"] == "full-test-1" for t in out2["tags"]) and lg["entries"][0]["id"] == hd["entries"][0]["id"], err[:80], ms)
    rc, _, err, _ = scn("checkout", "-b", "legacy-spelling", cwd=wc1)
    rc2, _, err2, _ = scn("checkout", "main", cwd=wc1)
    check("git spellings work and print a hint", rc == 0 and rc2 == 0 and "transitional" in err and "use `scn switch" in err2)
    scn("branch", "-d", "legacy-spelling", cwd=wc1)


def sparse_tests(wc_sparse):
    sec("9. Sparse checkout")
    rc, out, err, ms = scn("sync", cwd=wc_sparse, js=True, op="sync sparse checkout")
    check("a sparse checkout syncs many upstream commits", rc == 0, err[:100], ms)
    rc, out, err, ms = scn("sparse", "add", "api", cwd=wc_sparse, js=True, op="sparse add api")
    want = len([p for p in git("ls-files", "api").split("\n") if p])
    check("sparse add api checks out its files", rc == 0 and (wc_sparse / "api").exists(), f"{out.get('written') if rc == 0 else err} written, git has {want} at main", ms)
    f = next(p for p in tree_of(wc_sparse) if p.startswith("common/") and p.endswith(".go"))
    with open(wc_sparse / f, "a") as h:
        h.write("// from the sparse checkout\n")
    scn("commit", "-m", "sparse edit", cwd=wc_sparse)
    rc, _, err, ms = scn("land", cwd=wc_sparse, op="land from sparse")
    rc2, tip, _, _ = scn("show", "--name-only", cwd=wc_sparse, js=True)
    check("a commit from a sparse checkout touches only its own change", rc == 0 and [x["path"] for x in tip["diff"]["files"]] == [f], err[:100], ms)
    rc, out, err, ms = scn("sparse", "rm", "api", cwd=wc_sparse, js=True)
    check("sparse rm removes the directory", rc == 0 and not (wc_sparse / "api").exists(), err[:80], ms)
    rc, out, err, ms = scn("sparse", "disable", cwd=wc_sparse, js=True, op="sparse disable (full)")
    check("sparse disable fills in the whole tree", rc == 0 and len(tree_of(wc_sparse)) > 4000, f"{len(tree_of(wc_sparse))} paths", ms)


def bigfile_tests(wc1, wc2):
    sec("10. Large files and many small commits")
    big = wc1 / "assets" / "model.bin"
    big.parent.mkdir(exist_ok=True)
    big.write_bytes(os.urandom(40 << 20))
    text = wc1 / "assets" / "corpus.txt"
    text.write_text("".join(f"row {i}, value {i * 7 % 1000}\n" for i in range(1_500_000)))
    sizes = (big.stat().st_size, text.stat().st_size)
    rc, out, err, ms = scn("commit", "-m", "large files", cwd=wc1, op="commit 40 MB random + 32 MB text")
    rc2, _, err2, ms2 = scn("land", cwd=wc1, op="land large files")
    check("commit and land a 40 MB random file and a 32 MB text file", rc == 0 and rc2 == 0, f"{sizes[0] / 1e6:.0f} MB + {sizes[1] / 1e6:.0f} MB", ms + ms2)
    rc, data, err, ms = scn("cat", "main:assets/model.bin", cwd=wc1, op="cat 40 MB")
    check("cat reads the large file back byte for byte", rc == 0 and hashlib.sha256(data).digest() == hashlib.sha256(big.read_bytes()).digest(), "", ms)
    rc, out, err, ms = scn("sync", cwd=wc2, op="sync large files")
    check("sync brings the large files to the other working copy", rc == 0 and (wc2 / "assets/corpus.txt").read_bytes() == text.read_bytes(), "", ms)
    tot = 0
    for i in range(30):
        (wc1 / "CHANGELOG.scion").write_text(f"entry {i}\n")
        _, _, _, m1 = scn("commit", "-m", f"small {i}", cwd=wc1)
        rc, _, err, m2 = scn("land", cwd=wc1)
        tot += m1 + m2
        if rc != 0:
            break
    timed("commit + land, small change (avg of 30)", tot / 30)
    check("30 consecutive small commit + land cycles", rc == 0, f"{tot / 30:.0f} ms per cycle")


def server_tests():
    sec("11. Server: fsck, restart, storage")
    t = time.time()
    r = subprocess.run([str(B / "scion"), "admin", "fsck", "--lite", "--data-dir", str(S / "data"), "--repo", "starhub", "--verify"], capture_output=True, env=ENV)
    ms = (time.time() - t) * 1000
    timed("fsck", ms)
    check("fsck finds no problems", r.returncode == 0, (r.stdout + r.stderr).decode()[-200:].strip().replace("\n", " | "), ms)
    subprocess.run(["pkill", "-f", f"listen 127.0.0.1:{PORT}"])
    time.sleep(1)
    log = open(S / "server2.txt", "w")
    t = time.time()
    subprocess.Popen([str(B / "scion"), "server", "--lite", "--data-dir", str(S / "data"), "--listen", f"127.0.0.1:{PORT}"],
                     stdout=log, stderr=subprocess.STDOUT, env=ENV)
    while scn("doctor")[0] != 0 and time.time() - t < 30:
        time.sleep(0.1)
    ms = (time.time() - t) * 1000
    timed("server restart until ready", ms)
    rc, out, err, _ = scn("log", "-n", "1", cwd=S / "wc1", js=True)
    rc2, st, _, _ = scn("status", cwd=S / "wc1", js=True)
    check("after a restart: history and working copies still work", rc == 0 and rc2 == 0, "", ms)
    errs = [l for l in (S / "server.txt").read_text().split("\n") + (S / "server2.txt").read_text().split("\n") if " ERROR " in l or "panicked" in l]
    check("no errors or panics in the server logs", not errs, errs[0][:160] if errs else "")
    total = sum(f.stat().st_size for f in (S / "data/objects").rglob("*") if f.is_file())
    n = sum(1 for f in (S / "data/objects").rglob("*") if f.is_file())
    db = sum(f.stat().st_size for f in (S / "data").glob("meta.db*"))
    return total, n, db


def main():
    t0 = time.time()
    branches, tag = setup()
    wc1 = read_tests(tag)
    wc2 = S / "wc2"
    history_tests(wc1)
    mod = mass_write_tests(wc1, wc2)
    collab_tests(wc1, wc2, mod)
    conflict_tests(wc1, wc2, mod)
    branch_tests(wc1, mod, branches)
    undo_tests(wc1, mod)
    sparse_tests(S / "sparse")
    bigfile_tests(wc1, wc2)
    total, n, db = server_tests()
    subprocess.run(["pkill", "-f", f"listen 127.0.0.1:{PORT}"])
    passed = sum(1 for r in results if r[2])
    out = {"results": results, "timings": timings, "objects": [total, n], "meta_db": db, "elapsed_s": time.time() - t0,
           "branches": branches, "tag": tag}
    (S / "results.json").write_text(json.dumps(out, indent=1))
    print(f"\n{passed}/{len(results)} checks passed in {time.time() - t0:.0f} s; objects {total / 1e6:.0f} MB in {n} files; meta {db / 1e6:.0f} MB")


if __name__ == "__main__":
    main()
