#!/usr/bin/env python3
"""Workload plan shared by gitalybench and scionbench: revisions are git SHAs of the imported main."""
import json, random, subprocess, sys

G = sys.argv[1]
random.seed(42)


def git(*a):
    return subprocess.run(["git", "-C", G, *a], capture_output=True, text=True).stdout


revs = git("rev-list", "main").split()
merges = git("rev-list", "--merges", "main").split()
recent = revs[:3000]


def tree(rev):
    out = []
    for l in git("ls-tree", "-r", "-l", rev).split("\n"):
        if not l or l.split()[1] != "blob":
            continue
        meta, path = l.split("\t", 1)
        out.append((path, int(meta.split()[3])))
    return out


main_tree = tree(revs[0])
small = [p for p, s in main_tree if 200 < s < 20_000]
large = [p for p, s in sorted(main_tree, key=lambda x: -x[1]) if s > 100_000 and not p.endswith((".png", ".jpg", ".gif", ".ico"))][:15]
dirs_main = sorted({p.rsplit("/", 1)[0] for p, _ in main_tree if "/" in p})
deep = [d for d in dirs_main if d.count("/") >= 3]
text_files = [p for p in small if p.endswith((".go", ".md", ".yaml", ".yml", ".sql", ".ts", ".vue"))]

files = []
for _ in range(240):  # small files at random recent revisions
    r = random.choice(recent)
    t = [p for p, s in tree(r) if 200 < s < 20_000] if random.random() < 0.15 else None
    files.append({"rev": r if t else revs[0], "path": random.choice(t) if t else random.choice(small)})
for p in large:  # the largest files at main
    files.append({"rev": revs[0], "path": p})
random.shuffle(files)

dirs = [{"rev": revs[0], "path": ""}, {"rev": revs[1000], "path": ""}, {"rev": revs[3000], "path": ""}]
dirs += [{"rev": revs[0], "path": d} for d in random.sample(dirs_main, 25)]
dirs += [{"rev": revs[0], "path": d} for d in random.sample(deep, 12)]

diffs = []
for _ in range(15):  # neighbours: one commit apart
    i = random.randrange(1, 3000)
    diffs.append({"a": revs[i], "b": revs[i - 1]})
for _ in range(10):  # far apart
    i = random.randrange(500, 6000)
    diffs.append({"a": revs[i], "b": revs[random.randrange(0, 200)]})

blames = [{"rev": revs[0], "path": p} for p in random.sample(text_files, 20)]
big_text = [p for p, s in main_tree if s > 30_000 and p.endswith((".go", ".sql", ".md"))][:10]
blames += [{"rev": revs[0], "path": p} for p in big_text]

plan = {
    "files": files,
    "dirs": dirs,
    "logs": [revs[0]] + random.sample(recent, 29) + random.sample(revs[3000:], 10),
    "path_logs": [{"rev": revs[0], "path": p} for p in random.sample(text_files, 20)]
    + [{"rev": revs[0], "path": d} for d in random.sample(dirs_main, 10)],
    "commits": random.sample(recent, 30) + random.sample(merges[:2000], 10),
    "diffs": diffs,
    "blames": blames,
    "archives": [revs[0], revs[1000], revs[3000]],
    "write_files": random.sample(text_files, 30),
    "concurrent": 16,
    "per_worker": 10,
    "batch_size": 200,
    "reader_levels": [1, 8, 32, 64],
    "large_file_mb": 20,
}
json.dump(plan, open(sys.argv[2], "w"), indent=1)
print({k: len(v) if isinstance(v, list) else v for k, v in plan.items()})
