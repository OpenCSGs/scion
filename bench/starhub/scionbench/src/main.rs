//! scionbench: the same workload as gitalybench, against a Scion server through the Rust SDK.
//!     scionbench PLAN OUT
//! Commit ids in the results are git SHAs (GetGitShas), so they compare with Gitaly's.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use scion_client::{ArchiveFormat, Client, ObjectId};
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

#[derive(Default)]
struct Res {
    timings: BTreeMap<String, Vec<f64>>,
    values: BTreeMap<String, BTreeMap<String, Value>>,
    errors: BTreeMap<String, Vec<String>>,
}

type Shared = Arc<Mutex<Res>>;

async fn time<T, F: std::future::Future<Output = Result<T, scion_client::Error>>>(r: &Shared, op: &str, f: F) -> Option<T> {
    let t = Instant::now();
    let out = f.await;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut g = r.lock().unwrap();
    g.timings.entry(op.into()).or_default().push(ms);
    match out {
        Ok(v) => Some(v),
        Err(e) => {
            g.errors.entry(op.into()).or_default().push(e.to_string());
            None
        }
    }
}

fn put(r: &Shared, op: &str, key: &str, v: Value) {
    r.lock().unwrap().values.entry(op.into()).or_default().insert(key.into(), v);
}

fn digest(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// git SHAs for Scion commit ids (cached).
async fn shas(c: &Client, cache: &Mutex<HashMap<ObjectId, String>>, ids: &[ObjectId]) -> Vec<String> {
    let missing: Vec<ObjectId> = {
        let g = cache.lock().unwrap();
        let mut m: Vec<ObjectId> = ids.iter().filter(|i| !g.contains_key(i)).copied().collect();
        m.sort();
        m.dedup();
        m
    };
    for part in missing.chunks(1000) {
        if let Ok(s) = c.git_shas(part).await {
            let mut g = cache.lock().unwrap();
            for (i, sha) in part.iter().zip(s) {
                g.insert(*i, sha);
            }
        }
    }
    let g = cache.lock().unwrap();
    ids.iter().map(|i| g.get(i).cloned().unwrap_or_default()).collect()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let plan: Plan = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
    let endpoint = std::env::var("SCION_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:17080".into());
    let c = Client::builder().endpoint(&endpoint).repo("starhub").build().await.unwrap();
    let r: Shared = Arc::default();
    let cache = Mutex::new(HashMap::new());

    for f in &plan.files {
        if let Some(d) = time(&r, "read_file", c.read_file(&f.rev, &f.path)).await {
            put(&r, "read_file", &format!("{}:{}", f.rev, f.path), json!(digest(&d)));
        }
    }
    for d in &plan.dirs {
        if let Some(es) = time(&r, "list_dir", c.list_dir(&d.rev, &d.path)).await {
            let mut names: Vec<String> = es.into_iter().map(|e| e.name).collect();
            names.sort();
            put(&r, "list_dir", &format!("{}:{}", d.rev, d.path), json!(names));
        }
        if let Some(page) = time(&r, "last_commits", c.last_commits(&d.rev, &d.path, 0, 0)).await {
            let ids: Vec<ObjectId> = page.entries.iter().filter_map(|(_, id)| *id).collect();
            let s = shas(&c, &cache, &ids).await;
            let m: BTreeMap<String, String> = page
                .entries
                .iter()
                .filter(|(_, id)| id.is_some())
                .map(|(n, _)| n.clone())
                .zip(s)
                .collect();
            put(&r, "last_commits", &format!("{}:{}", d.rev, d.path), json!(m));
        }
    }
    for rev in &plan.logs {
        if let Some(p) = time(&r, "log_50", c.log(rev, None, 50, None)).await {
            let ids: Vec<ObjectId> = p.entries.iter().map(|e| e.id).collect();
            put(&r, "log_50", rev, json!(shas(&c, &cache, &ids).await));
        }
    }
    for p in &plan.path_logs {
        if let Some(pg) = time(&r, "path_log_50", c.log(&p.rev, Some(&p.path), 50, None)).await {
            let ids: Vec<ObjectId> = pg.entries.iter().map(|e| e.id).collect();
            put(&r, "path_log_50", &format!("{}:{}", p.rev, p.path), json!(shas(&c, &cache, &ids).await));
        }
    }
    for rev in &plan.commits {
        // Like starhub's GetSingleCommit: the commit and its changes against the first parent
        let t = Instant::now();
        let mut ok = false;
        if let Ok(p) = c.log(rev, None, 1, None).await {
            if let Some(e) = p.entries.first() {
                let from = e.parents.first().map(|x| x.to_string());
                if c.diff(from.as_deref(), &e.id.to_string(), None).await.is_ok() {
                    ok = true;
                    put(&r, "show_commit", rev, json!(shas(&c, &cache, &[e.id]).await[0]));
                }
            }
        }
        let mut g = r.lock().unwrap();
        g.timings.entry("show_commit".into()).or_default().push(t.elapsed().as_secs_f64() * 1000.0);
        if !ok {
            g.errors.entry("show_commit".into()).or_default().push(rev.clone());
        }
    }
    for d in &plan.diffs {
        if let Some(fs) = time(&r, "diff_names", c.diff(Some(&d.a), &d.b, None)).await {
            let mut names: Vec<String> = fs.into_iter().map(|f| f.path).collect();
            names.sort();
            put(&r, "diff_names", &format!("{}..{}", d.a, d.b), json!(names));
        }
    }
    for b in &plan.blames {
        if let Some(info) = time(&r, "blame", c.blame(&b.rev, &b.path)).await {
            let ids: Vec<ObjectId> = info.commits.iter().map(|x| x.id).collect();
            let s = shas(&c, &cache, &ids).await;
            let per: Vec<String> = info.lines.iter().map(|i| s[*i].clone()).collect();
            put(&r, "blame", &format!("{}:{}", b.rev, b.path), json!(per));
        }
    }
    for rev in &plan.archives {
        let mut buf = Vec::new();
        if let Some((_, n)) = time(&r, "archive_zip", c.archive(rev, "", ArchiveFormat::Zip, "", &mut buf)).await {
            put(&r, "archive_zip", rev, json!(n));
            std::fs::write(format!("{}.scion-{rev}.zip", args[2]), &buf).ok();
        }
    }
    if let Some(st) = time(&r, "repo_size", c.tree_stats("main", "")).await {
        put(&r, "repo_size", "main", json!(st.bytes));
    }
    if let Some(refs) = time(&r, "branches", c.list_refs()).await {
        put(&r, "branches", "count", json!(refs.iter().filter(|(n, _, _)| !n.starts_with("tags/")).count()));
    }

    // Read throughput at increasing concurrency
    for &level in &plan.reader_levels {
        let t = Instant::now();
        let files = Arc::new(plan.files.clone());
        let mut tasks = Vec::new();
        for w in 0..level {
            let (c, files, r) = (c.clone(), files.clone(), r.clone());
            tasks.push(tokio::spawn(async move {
                let mut i = w;
                while i < files.len() {
                    let f = &files[i];
                    time(&r, &format!("read_concurrency_{level}"), c.read_file(&f.rev, &f.path)).await;
                    i += level;
                }
            }));
        }
        futures::future::join_all(tasks).await;
        put(&r, &format!("read_concurrency_{level}"), "wall_ms", json!(t.elapsed().as_millis()));
    }

    // ---- writes: create a commit from content, then land it (the scion equivalent of UserCommitFiles)
    async fn commit(c: &Client, files: Vec<(String, Vec<u8>)>, msg: &str) -> Result<(), scion_client::Error> {
        let tip = c.resolve_ref("main").await?.map(|t| t.changeset);
        let mut b = c.changeset(tip).message(msg).author("tester", "tester@example.com");
        for (p, d) in files {
            b = b.put(p, d);
        }
        let cs = b.create().await?;
        c.land("main", &cs.id).await?;
        Ok(())
    }
    for (i, p) in plan.write_files.iter().enumerate() {
        time(&r, "commit_1_file", commit(&c, vec![(p.clone(), format!("edited {i}\n").into_bytes())], &format!("edit {i}"))).await;
    }
    for round in 0..3 {
        let files = (0..plan.batch_size)
            .map(|i| (format!("bench/batch{round}/f{i}.txt"), format!("batch {round} file {i}\n").into_bytes()))
            .collect();
        time(&r, "commit_batch", commit(&c, files, &format!("batch {round}"))).await;
    }
    let big: Vec<u8> = (0..plan.large_file_mb << 20).map(|i| (i * 2654435761usize >> 13) as u8).collect();
    time(&r, "commit_large_file", commit(&c, vec![("bench/large.bin".into(), big)], "large file")).await;
    let t = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..plan.concurrent {
        let (c, r, per) = (c.clone(), r.clone(), plan.per_worker);
        tasks.push(tokio::spawn(async move {
            for i in 0..per {
                time(&r, "concurrent_commit", commit(&c, vec![(format!("bench/w{w}/c{i}.txt"), b"x\n".to_vec())], &format!("w{w} c{i}"))).await;
            }
        }));
    }
    futures::future::join_all(tasks).await;
    put(&r, "concurrent_commit", "wall_ms", json!(t.elapsed().as_millis()));
    let mut survived = 0;
    if let Ok(es) = c.list_dir("main", "bench").await {
        for e in es.iter().filter(|e| e.name.starts_with('w')) {
            survived += c.list_dir("main", &format!("bench/{}", e.name)).await.map(|v| v.len()).unwrap_or(0);
        }
    }
    put(&r, "concurrent_commit", "files_on_main", json!(survived));
    let tip = c.resolve_ref("main").await.ok().flatten().map(|t| t.changeset);
    for i in 0..20 {
        if let Some(tip) = tip {
            time(&r, "branch_create", c.create_ref(&format!("bench-{i}"), &tip)).await;
        }
    }
    for i in 0..20 {
        time(&r, "branch_delete", c.delete_ref(&format!("bench-{i}"), None)).await;
    }

    let g = r.lock().unwrap();
    let out = json!({"timings": g.timings, "values": g.values, "errors": g.errors});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&out).unwrap()).unwrap();
    println!("done");
}
