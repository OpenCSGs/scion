//! gitbench: the comparison workload directly on a bare git repository, in process through
//! libgit2, with no server or network in between. A lower bound for what a git-based service can
//! do. Where libgit2 has no API (last commit per entry, path history, archives) it runs the git
//! command line, as a git server would.
//!
//!     gitbench REPO PLAN OUT

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use git2::{BlameOptions, BranchType, Oid, Repository, Signature, Sort, Tree};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize, Clone)]
struct RevPath {
    rev: String,
    path: String,
}
#[derive(Deserialize, Clone)]
struct Pair {
    a: String,
    b: String,
}
#[derive(Deserialize, Clone)]
struct Plan {
    files: Vec<RevPath>,
    dirs: Vec<RevPath>,
    logs: Vec<String>,
    path_logs: Vec<RevPath>,
    commits: Vec<String>,
    diffs: Vec<Pair>,
    blames: Vec<RevPath>,
    archives: Vec<String>,
    write_files: Vec<String>,
    concurrent: usize,
    per_worker: usize,
    batch_size: usize,
    reader_levels: Vec<usize>,
    large_file_mb: usize,
}

type Res = Arc<Mutex<Out>>;
type R<T> = Result<T, String>;

#[derive(Default)]
struct Out {
    timings: BTreeMap<String, Vec<f64>>,
    values: BTreeMap<String, BTreeMap<String, Value>>,
    errors: BTreeMap<String, Vec<String>>,
}

fn time<T>(r: &Res, op: &str, f: impl FnOnce() -> R<T>) -> Option<T> {
    let t = Instant::now();
    let out = f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut g = r.lock().unwrap();
    g.timings.entry(op.into()).or_default().push(ms);
    match out {
        Ok(v) => Some(v),
        Err(e) => {
            g.errors.entry(op.into()).or_default().push(e);
            None
        }
    }
}

fn put(r: &Res, op: &str, key: &str, v: Value) {
    r.lock().unwrap().values.entry(op.into()).or_default().insert(key.into(), v);
}

fn e(err: git2::Error) -> String {
    err.message().to_owned()
}

fn tree_at<'r>(repo: &'r Repository, rev: &str, path: &str) -> R<Tree<'r>> {
    let root = repo.revparse_single(rev).map_err(e)?.peel_to_tree().map_err(e)?;
    if path.is_empty() {
        return Ok(root);
    }
    let entry = root.get_path(Path::new(path)).map_err(e)?;
    entry.to_object(repo).map_err(e)?.peel_to_tree().map_err(e)
}

fn read_file(repo: &Repository, rev: &str, path: &str) -> R<Vec<u8>> {
    let root = repo.revparse_single(rev).map_err(e)?.peel_to_tree().map_err(e)?;
    let entry = root.get_path(Path::new(path)).map_err(e)?;
    Ok(repo.find_blob(entry.id()).map_err(e)?.content().to_vec())
}

fn git(repo: &str, args: &[&str]) -> R<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|x| x.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(out.stdout)
}

/// The last commit of each entry of a directory, in one `git log` pass (git's own history
/// simplification): the first commit that names a path under an entry is that entry's last.
fn last_commits(repo: &Repository, dir: &str, rev: &str, path: &str) -> R<BTreeMap<String, String>> {
    let names: HashSet<String> = tree_at(repo, rev, path)?
        .iter()
        .filter_map(|x| x.name().map(str::to_owned))
        .collect();
    let spec = if path.is_empty() { ".".to_owned() } else { path.to_owned() };
    let mut child = Command::new("git")
        .args(["-C", dir, "log", "--format=%x00%H", "--name-only", rev, "--", &spec])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|x| x.to_string())?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    let mut found = BTreeMap::new();
    let mut cur = String::new();
    use std::io::BufRead;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.map_err(|x| x.to_string())?;
        if let Some(sha) = line.strip_prefix('\0') {
            cur = sha.to_owned();
            continue;
        }
        let Some(rest) = line.strip_prefix(&prefix) else { continue };
        let entry = rest.split('/').next().unwrap_or_default();
        if names.contains(entry) && !found.contains_key(entry) {
            found.insert(entry.to_owned(), cur.clone());
            if found.len() == names.len() {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(found)
}

fn log(repo: &Repository, rev: &str, n: usize) -> R<Vec<String>> {
    let mut walk = repo.revwalk().map_err(e)?;
    walk.set_sorting(Sort::TIME).map_err(e)?;
    walk.push(repo.revparse_single(rev).map_err(e)?.id()).map_err(e)?;
    walk.take(n).map(|x| x.map(|o| o.to_string()).map_err(e)).collect()
}

fn changed_paths(repo: &Repository, a: Option<&Tree>, b: &Tree) -> R<Vec<String>> {
    let d = repo.diff_tree_to_tree(a, Some(b), None).map_err(e)?;
    let mut names: Vec<String> = d
        .deltas()
        .filter_map(|x| x.new_file().path().or(x.old_file().path()))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

fn blame(repo: &Repository, rev: &str, path: &str) -> R<Vec<String>> {
    let mut opts = BlameOptions::new();
    opts.newest_commit(repo.revparse_single(rev).map_err(e)?.id());
    let b = repo.blame_file(Path::new(path), Some(&mut opts)).map_err(e)?;
    let mut per = Vec::new();
    for h in b.iter() {
        for _ in 0..h.lines_in_hunk() {
            per.push(h.final_commit_id().to_string());
        }
    }
    Ok(per)
}

/// `tree` with `path` set to `blob` (subtrees created as needed).
fn upsert(repo: &Repository, tree: Option<&Tree>, path: &[&str], blob: Oid) -> R<Oid> {
    let mut tb = repo.treebuilder(tree).map_err(e)?;
    if path.len() == 1 {
        tb.insert(path[0], blob, 0o100644).map_err(e)?;
    } else {
        let sub = tree
            .and_then(|t| t.get_name(path[0]))
            .and_then(|x| x.to_object(repo).ok())
            .and_then(|o| o.into_tree().ok());
        let id = upsert(repo, sub.as_ref(), &path[1..], blob)?;
        tb.insert(path[0], id, 0o040000).map_err(e)?;
    }
    tb.write().map_err(e)
}

/// One commit on main with `files`, moving main only if it still points at the commit built on
/// (like a server-side commit API). `retry` rebuilds on the new tip when main moved meanwhile.
fn commit(repo: &Repository, files: &[(String, Vec<u8>)], msg: &str, retry: bool) -> R<()> {
    let sig = Signature::now("tester", "tester@example.com").map_err(e)?;
    let blobs: Vec<(String, Oid)> = files
        .iter()
        .map(|(p, d)| Ok((p.clone(), repo.blob(d).map_err(e)?)))
        .collect::<R<_>>()?;
    loop {
        let tip = repo.refname_to_id("refs/heads/main").map_err(e)?;
        let parent = repo.find_commit(tip).map_err(e)?;
        let mut tree = parent.tree().map_err(e)?;
        for (p, id) in &blobs {
            let comps: Vec<&str> = p.split('/').collect();
            tree = repo.find_tree(upsert(repo, Some(&tree), &comps, *id)?).map_err(e)?;
        }
        let new = repo.commit(None, &sig, &sig, msg, &tree, &[&parent]).map_err(e)?;
        match repo.reference_matching("refs/heads/main", new, true, tip, msg) {
            Ok(_) => return Ok(()),
            // main moved, or another writer holds its lock
            Err(err) if retry && matches!(err.code(), git2::ErrorCode::Modified | git2::ErrorCode::Locked) => continue,
            Err(err) => return Err(e(err)),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args[1].clone();
    let plan: Plan = serde_json::from_slice(&std::fs::read(&args[2]).unwrap()).unwrap();
    let repo = Repository::open_bare(&dir).unwrap();
    let r: Res = Arc::default();

    for f in &plan.files {
        if let Some(d) = time(&r, "read_file", || read_file(&repo, &f.rev, &f.path)) {
            put(&r, "read_file", &format!("{}:{}", f.rev, f.path), json!(hex::encode(Sha256::digest(&d))));
        }
    }
    for d in &plan.dirs {
        let key = format!("{}:{}", d.rev, d.path);
        let names = time(&r, "list_dir", || {
            let mut n: Vec<String> = tree_at(&repo, &d.rev, &d.path)?
                .iter()
                .filter_map(|x| x.name().map(str::to_owned))
                .collect();
            n.sort();
            Ok(n)
        });
        if let Some(n) = names {
            put(&r, "list_dir", &key, json!(n));
        }
        if let Some(m) = time(&r, "last_commits", || last_commits(&repo, &dir, &d.rev, &d.path)) {
            put(&r, "last_commits", &key, json!(m));
        }
    }
    for rev in &plan.logs {
        if let Some(ids) = time(&r, "log_50", || log(&repo, rev, 50)) {
            put(&r, "log_50", rev, json!(ids));
        }
    }
    for p in &plan.path_logs {
        let ids = time(&r, "path_log_50", || {
            let out = git(&dir, &["log", "-50", "--topo-order", "--format=%H", &p.rev, "--", &p.path])?;
            Ok(String::from_utf8_lossy(&out).split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        });
        if let Some(ids) = ids {
            put(&r, "path_log_50", &format!("{}:{}", p.rev, p.path), json!(ids));
        }
    }
    for rev in &plan.commits {
        // The commit and the files it changes against its first parent
        let id = time(&r, "show_commit", || {
            let c = repo.revparse_single(rev).map_err(e)?.peel_to_commit().map_err(e)?;
            let parent = c.parents().next().map(|p| p.tree()).transpose().map_err(e)?;
            changed_paths(&repo, parent.as_ref(), &c.tree().map_err(e)?)?;
            Ok(c.id().to_string())
        });
        if let Some(id) = id {
            put(&r, "show_commit", rev, json!(id));
        }
    }
    for d in &plan.diffs {
        let names = time(&r, "diff_names", || {
            let a = tree_at(&repo, &d.a, "")?;
            changed_paths(&repo, Some(&a), &tree_at(&repo, &d.b, "")?)
        });
        if let Some(n) = names {
            put(&r, "diff_names", &format!("{}..{}", d.a, d.b), json!(n));
        }
    }
    for b in &plan.blames {
        if let Some(per) = time(&r, "blame", || blame(&repo, &b.rev, &b.path)) {
            put(&r, "blame", &format!("{}:{}", b.rev, b.path), json!(per));
        }
    }
    // git CLI blame, for comparison with libgit2's
    for b in &plan.blames {
        time(&r, "blame_cli", || git(&dir, &["blame", "--porcelain", &b.rev, "--", &b.path]));
    }
    for rev in &plan.archives {
        if let Some(z) = time(&r, "archive_zip", || git(&dir, &["archive", "--format=zip", rev])) {
            put(&r, "archive_zip", rev, json!(z.len()));
            std::fs::write(format!("{}.git-{rev}.zip", args[3]), &z).ok();
        }
    }
    if let Some(n) = time(&r, "branches", || {
        Ok(repo.branches(Some(BranchType::Local)).map_err(e)?.count())
    }) {
        put(&r, "branches", "count", json!(n));
    }

    // Read throughput at increasing concurrency: one repository handle per thread
    for &level in &plan.reader_levels {
        let t = Instant::now();
        let files = Arc::new(plan.files.clone());
        let threads: Vec<_> = (0..level)
            .map(|w| {
                let (files, r, dir) = (files.clone(), r.clone(), dir.clone());
                std::thread::spawn(move || {
                    let repo = Repository::open_bare(&dir).unwrap();
                    let op = format!("read_concurrency_{level}");
                    let mut i = w;
                    while i < files.len() {
                        time(&r, &op, || read_file(&repo, &files[i].rev, &files[i].path));
                        i += level;
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        put(&r, &format!("read_concurrency_{level}"), "wall_ms", json!(t.elapsed().as_millis()));
    }

    // ---- writes
    for (i, p) in plan.write_files.iter().enumerate() {
        let files = vec![(p.clone(), format!("edited {i}\n").into_bytes())];
        time(&r, "commit_1_file", || commit(&repo, &files, &format!("edit {i}"), false));
    }
    for round in 0..3 {
        let files: Vec<_> = (0..plan.batch_size)
            .map(|i| (format!("bench/batch{round}/f{i}.txt"), format!("batch {round} file {i}\n").into_bytes()))
            .collect();
        time(&r, "commit_batch", || commit(&repo, &files, &format!("batch {round}"), false));
    }
    let big: Vec<u8> = (0..plan.large_file_mb << 20).map(|i| (i * 2654435761usize >> 13) as u8).collect();
    time(&r, "commit_large_file", || {
        commit(&repo, &[("bench/large.bin".into(), big.clone())], "large file", false)
    });
    for (op, retry) in [("concurrent_commit", false), ("concurrent_commit_retry", true)] {
        let t = Instant::now();
        let threads: Vec<_> = (0..plan.concurrent)
            .map(|w| {
                let (r, dir, per) = (r.clone(), dir.clone(), plan.per_worker);
                std::thread::spawn(move || {
                    let repo = Repository::open_bare(&dir).unwrap();
                    for i in 0..per {
                        let files = vec![(format!("bench/{op}/w{w}/c{i}.txt"), b"x\n".to_vec())];
                        time(&r, op, || commit(&repo, &files, &format!("w{w} c{i}"), retry));
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        put(&r, op, "wall_ms", json!(t.elapsed().as_millis()));
        let survived = tree_at(&repo, "main", &format!("bench/{op}"))
            .map(|t| {
                t.iter()
                    .filter_map(|w| w.to_object(&repo).ok()?.into_tree().ok().map(|s| s.len()))
                    .sum::<usize>()
            })
            .unwrap_or(0);
        put(&r, op, "files_on_main", json!(survived));
    }
    let tip = repo.refname_to_id("refs/heads/main").unwrap();
    let tip = repo.find_commit(tip).unwrap();
    for i in 0..20 {
        time(&r, "branch_create", || repo.branch(&format!("bench-{i}"), &tip, false).map(|_| ()).map_err(e));
    }
    for i in 0..20 {
        time(&r, "branch_delete", || {
            repo.find_branch(&format!("bench-{i}"), BranchType::Local)
                .and_then(|mut b| b.delete())
                .map_err(e)
        });
    }

    let g = r.lock().unwrap();
    let out = json!({"timings": g.timings, "values": g.values, "errors": g.errors});
    std::fs::write(&args[3], serde_json::to_vec_pretty(&out).unwrap()).unwrap();
    println!("done");
}
