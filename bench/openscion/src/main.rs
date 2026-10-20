//! The four synthetic Git/Scion workloads of OpenScion's benchmarks, as CarterT27/scion's runner
//! (`crates/scion-conformance/examples/benchmark.rs`) times them, so the three can be compared on
//! one machine.
//!
//! 10 directories of `files-per-directory` files of 4 KiB of pseudorandom bytes. Per run: a fresh
//! fixture, a fresh in-process server and fresh working copies; then, timed one by one:
//!
//! - import: `git init` + `add -A` + `commit`; Scion uploads the files in batches of 256 and
//!   creates a parentless changeset and a ref;
//! - full checkout: `git clone --no-local`; Scion's working-copy clone;
//! - one-directory checkout: `git clone --no-local --filter=blob:none --sparse` and
//!   `sparse-checkout set dir0000`; Scion's clone of `dir0000` only;
//! - commit one changed file: `git commit -a`; Scion's working-copy commit (status scan, upload,
//!   changeset; no land, as on the Git side no push).
//!
//! Fixture writes and checks of the checked-out files are outside the timers.
//!
//! ```sh
//! cargo run --release -- 100 7     # 1,000 files, 7 runs
//! cargo run --release -- 1000 5    # 10,000 files, 5 runs
//! ```
//!
//! A third argument picks the server's storage: `objects` (default: the object store in memory,
//! with zstd compression and packs, as on disk or S3) or `plain` (objects in memory as they are).
//! A fourth, `auth`, makes the server require authentication: the client exchanges a PAT for
//! short-lived access tokens, as `scn` does, and every request carries one.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use scion_api::scion_auth::{Guard, GuardConfig};
use scion_api::{App, ServerConfig, serve};
use scion_client::{Auth, Client};
use scion_store::{BlobStore, MemStore, MetaStore};
use scion_store_blob::ObjectBlobStore;
use scion_wc::{Sparse, WorkingCopy};
use tokio::net::TcpListener;

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(["-c", "user.name=Benchmark", "-c", "user.email=benchmark@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// Pseudorandom file contents (xorshift; incompressible, like the original's ChaCha8 bytes).
fn fixture(files_per_dir: usize) -> Vec<(String, Vec<u8>)> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..10)
        .flat_map(|d| (0..files_per_dir).map(move |f| (d, f)))
        .map(|(d, f)| {
            let content: Vec<u8> = (0..512).flat_map(|_| next().to_le_bytes()).collect();
            (format!("dir{d:04}/file{f:05}.bin"), content)
        })
        .collect()
}

fn write_fixture(root: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    for (path, bytes) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().context("file parent")?)?;
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

fn verify(root: &Path, files: &[(String, Vec<u8>)], sparse: bool) -> Result<()> {
    for (path, bytes) in files {
        if sparse && !path.starts_with("dir0000/") {
            ensure!(!root.join(path).exists(), "out-of-scope file: {path}");
        } else {
            ensure!(std::fs::read(root.join(path))? == *bytes, "content mismatch: {path}");
        }
    }
    Ok(())
}

struct Server {
    url: String,
    /// A PAT when the server requires authentication
    token: Option<String>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Server {
    async fn client(&self) -> Result<Client> {
        let b = Client::builder().endpoint(&self.url).repo("bench");
        let b = match &self.token {
            Some(t) => b.auth(Auth::Exchange(t.clone())),
            None => b,
        };
        Ok(b.build().await?)
    }
}

async fn server(objects: bool, auth: bool) -> Result<Server> {
    let store = Arc::new(MemStore::new());
    store.create_repo("bench").await?;
    let meta: Arc<dyn MetaStore> = store.clone();
    let blobs: Arc<dyn BlobStore> = if objects {
        Arc::new(ObjectBlobStore::memory().with_packs(meta.clone()))
    } else {
        store
    };
    let guard = GuardConfig {
        required: auth,
        signing_key_pem: auth.then(scion_api::scion_auth::token::generate_signing_key),
        ..GuardConfig::default()
    };
    let token = if auth {
        let g = Guard::build(guard.clone(), meta.clone(), Arc::new(|_: &str| None)).map_err(anyhow::Error::msg)?;
        Some(g.create_token("bench", "bench", vec![], "", None).await?.0)
    } else {
        None
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let app = App {
        config: Arc::new(ServerConfig {
            auth: guard,
            ..ServerConfig::default()
        }),
        meta,
        blobs,
    };
    tokio::spawn(serve(listener, app, async {
        let _ = rx.await;
    }));
    Ok(Server { url, token, _shutdown: tx })
}

/// Directory ingestion through the write API (not the Git-history importer).
async fn import(client: &Client, root: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    let contents: Vec<Bytes> = files
        .iter()
        .map(|(path, _)| std::fs::read(root.join(path)).map(Bytes::from))
        .collect::<std::io::Result<_>>()?;
    for batch in contents.chunks(256) {
        client.upload_blobs(batch).await?;
    }
    let mut cs = client.changeset(None).message("import").author("Benchmark", "benchmark@example.com");
    for ((path, _), content) in files.iter().zip(contents) {
        cs = cs.put(path.clone(), content);
    }
    let created = cs.create().await?;
    client.create_ref("main", &created.id).await?;
    Ok(())
}

fn median(samples: &mut [Duration]) -> f64 {
    samples.sort_unstable();
    let m = samples.len() / 2;
    let s = if samples.len() % 2 == 0 {
        (samples[m - 1].as_secs_f64() + samples[m].as_secs_f64()) / 2.0
    } else {
        samples[m].as_secs_f64()
    };
    s * 1000.0
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files_per_dir: usize = args.first().map(|v| v.parse()).transpose()?.unwrap_or(100);
    let runs: usize = args.get(1).map(|v| v.parse()).transpose()?.unwrap_or(5);
    let objects = match args.get(2).map(String::as_str) {
        None | Some("objects") => true,
        Some("plain") => false,
        Some(other) => anyhow::bail!("storage is objects or plain, not {other}"),
    };
    let auth = match args.get(3).map(String::as_str) {
        None => false,
        Some("auth") => true,
        Some(other) => anyhow::bail!("the fourth argument is auth, not {other}"),
    };
    ensure!(files_per_dir > 0 && runs > 0, "sizes and runs must be positive");
    let files = fixture(files_per_dir);
    // git import, scion import, git full, scion full, git sparse, scion sparse, git commit, scion commit
    let mut samples: Vec<Vec<Duration>> = vec![Vec::new(); 8];
    for run in 0..runs {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path();
        let source = root.join("source");
        write_fixture(&source, &files)?;
        let srv = server(objects, auth).await?;
        let client = srv.client().await?;

        let t = Instant::now();
        git(&source, &["init", "-q", "-b", "main"])?;
        git(&source, &["add", "-A"])?;
        git(&source, &["commit", "-q", "-m", "initial"])?;
        samples[0].push(t.elapsed());
        git(&source, &["config", "uploadpack.allowFilter", "true"])?;
        let t = Instant::now();
        import(&client, &source, &files).await?;
        samples[1].push(t.elapsed());

        let t = Instant::now();
        git(root, &["clone", "-q", "--no-local", "source", "git-full"])?;
        samples[2].push(t.elapsed());
        let t = Instant::now();
        let mut wc = WorkingCopy::clone(&client, &srv.url, "bench", "main", &root.join("scion-full"), Sparse::full())
            .await?;
        samples[3].push(t.elapsed());
        verify(&root.join("git-full"), &files, false)?;
        verify(&root.join("scion-full"), &files, false)?;

        let t = Instant::now();
        git(root, &["clone", "-q", "--no-local", "--filter=blob:none", "--sparse", "source", "git-sparse"])?;
        git(&root.join("git-sparse"), &["sparse-checkout", "set", "dir0000"])?;
        samples[4].push(t.elapsed());
        let t = Instant::now();
        let dir = Sparse::dirs(&["dir0000"])?;
        WorkingCopy::clone(&client, &srv.url, "bench", "main", &root.join("scion-sparse"), dir).await?;
        samples[5].push(t.elapsed());
        verify(&root.join("git-sparse"), &files, true)?;
        verify(&root.join("scion-sparse"), &files, true)?;

        let mut changed = files[0].1.clone();
        changed[0] ^= 1;
        std::fs::write(root.join("git-full").join(&files[0].0), &changed)?;
        std::fs::write(root.join("scion-full").join(&files[0].0), &changed)?;
        let t = Instant::now();
        git(&root.join("git-full"), &["commit", "-q", "-a", "-m", "one file"])?;
        samples[6].push(t.elapsed());
        let t = Instant::now();
        let created = wc.commit(&client, "one file", Some(("Benchmark", "benchmark@example.com"))).await?;
        samples[7].push(t.elapsed());
        ensure!(created.is_some(), "the commit must change one file");
        ensure!(wc.status()?.is_clean(), "the commit left dirty files");
        eprintln!(
            "run {}: {:?} ms (git/scion: import, full, sparse, commit)",
            run + 1,
            samples.iter().map(|v| (v[run].as_secs_f64() * 1e4).round() / 10.0).collect::<Vec<_>>()
        );
    }
    let storage = if objects { "object store in memory" } else { "plain memory" };
    let auth = if auth { ", authentication required" } else { "" };
    println!("{} files, 4096 bytes/file, {runs} runs, {storage}{auth}; medians:", files.len());
    println!("| Scenario | Git (ms) | Scion (ms) |\n| --- | ---: | ---: |");
    for (name, i) in [("import", 0), ("commit-one-file", 6), ("full-clone", 2), ("sparse-clone", 4)] {
        println!("| {name} | {:.1} | {:.1} |", median(&mut samples[i]), median(&mut samples[i + 1]));
    }
    Ok(())
}
