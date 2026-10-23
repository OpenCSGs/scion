//! The git gateway with the real git CLI: clone, fetch and pull over smart HTTP (protocol v0 and
//! v2), SHAs equal to GetGitShas, and the errors for what it does not serve. Skipped without git.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use scion_api::scion_auth::{AuthConfig, Guard, GuardConfig};
use scion_api::{App, ServerConfig, serve};
use scion_client::{Auth, Client, EntryMode};
use scion_store::{MemStore, MetaStore};
use tokio::net::TcpListener;

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs git with no user or system configuration and no proxy.
async fn git(dir: &Path, args: &[&str]) -> Output {
    let dir = dir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("no_proxy", "*")
            .env("LC_ALL", "C")
            .output()
            .expect("run git")
    })
    .await
    .expect("join")
}

async fn ok(dir: &Path, args: &[&str]) -> String {
    let o = git(dir, args).await;
    assert!(
        o.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

async fn fails(dir: &Path, args: &[&str]) -> String {
    let o = git(dir, args).await;
    assert!(!o.status.success(), "git {args:?} succeeded");
    String::from_utf8_lossy(&o.stderr).into_owned()
}

async fn start(config: ServerConfig, store: Arc<MemStore>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let app = App {
        config: Arc::new(ServerConfig {
            warm_record_cache: false,
            ..config
        }),
        meta: store.clone(),
        blobs: store,
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, app, async {
        let _ = rx.await;
    }));
    (url, tx)
}

#[tokio::test(flavor = "multi_thread")]
async fn git_clones_fetches_and_pulls() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let (url, _stop) = start(ServerConfig::default(), store.clone()).await;
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .build()
        .await
        .expect("connect");
    let a = c
        .changeset(None)
        .put("README.md", "hello\n")
        .put("src/lib.rs", "pub fn f() {}\n")
        .put_with_mode("run.sh", EntryMode::Executable, "#!/bin/sh\necho hi\n")
        .put_with_mode("link", EntryMode::Symlink, "README.md")
        .message("first\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &a.id).await.expect("land");
    let b = c
        .changeset(Some(a.id))
        .put("src/lib.rs", "pub fn f() -> u8 { 1 }\n")
        .put("docs/guide.md", "# Guide\n")
        .message("second\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &b.id).await.expect("land");
    c.create_ref("feature", &a.id).await.expect("branch");
    c.create_ref("tags/v1", &b.id).await.expect("tag");

    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let shas = c.git_shas(&[a.id, b.id]).await.expect("shas");

    // ls-remote lists HEAD, branches and tags with the SHAs GetGitShas gives
    let refs = ok(root, &["ls-remote", &format!("{url}/mono")]).await;
    assert!(refs.contains(&format!("{}\tHEAD", shas[1])), "{refs}");
    assert!(refs.contains(&format!("{}\trefs/heads/main", shas[1])), "{refs}");
    assert!(refs.contains(&format!("{}\trefs/heads/feature", shas[0])), "{refs}");
    assert!(refs.contains(&format!("{}\trefs/tags/v1", shas[1])), "{refs}");

    // Protocol v2 (git's default) and v0, with and without .git
    for (dir, version, path) in [("v2", "2", "mono"), ("v0", "0", "mono.git")] {
        ok(
            root,
            &[
                "-c",
                &format!("protocol.version={version}"),
                "clone",
                "-q",
                &format!("{url}/{path}"),
                dir,
            ],
        )
        .await;
        let w = root.join(dir);
        assert_eq!(
            std::fs::read_to_string(w.join("src/lib.rs")).expect("read"),
            "pub fn f() -> u8 { 1 }\n"
        );
        let log = ok(&w, &["log", "--format=%H %s"]).await;
        assert_eq!(log, format!("{} second\n{} first\n", shas[1], shas[0]));
        ok(&w, &["fsck", "--strict", "--no-dangling"]).await;
        let files = ok(&w, &["ls-files", "-s"]).await;
        assert!(files.contains("100755 ") && files.contains("\trun.sh"), "{files}");
        assert!(files.contains("120000 ") && files.contains("\tlink"), "{files}");
        let head = ok(&w, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        assert_eq!(head.trim(), "main");
    }

    // A new commit lands: fetch sends only what is new, pull fast-forwards
    let d = c
        .changeset(Some(b.id))
        .put("README.md", "hello again\n")
        .delete("docs/guide.md")
        .message("third\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &d.id).await.expect("land");
    let w = root.join("v2");
    let fetched = git(&w, &["fetch", "-v", "origin"]).await;
    assert!(fetched.status.success(), "{}", String::from_utf8_lossy(&fetched.stderr));
    ok(&w, &["pull", "-q", "--ff-only"]).await;
    assert_eq!(
        std::fs::read_to_string(w.join("README.md")).expect("read"),
        "hello again\n"
    );
    assert!(!w.join("docs/guide.md").exists());
    let sha = c.git_shas(&[d.id]).await.expect("sha").remove(0);
    assert_eq!(ok(&w, &["rev-parse", "HEAD"]).await.trim(), sha);
    ok(&w, &["fsck", "--strict", "--no-dangling"]).await;
    // The same over v0
    let w0 = root.join("v0");
    ok(&w0, &["-c", "protocol.version=0", "pull", "-q", "--ff-only"]).await;
    assert_eq!(ok(&w0, &["rev-parse", "HEAD"]).await.trim(), sha);

    // Up to date: nothing to fetch
    ok(&w, &["fetch", "-q", "origin"]).await;

    // What the gateway does not do yet says so
    let shallow = fails(
        root,
        &["clone", "-q", "--depth", "1", &format!("{url}/mono"), "shallow"],
    )
    .await;
    // git sees no `shallow` capability and refuses on its own
    assert!(shallow.contains("shallow"), "{shallow}");
    let missing = fails(root, &["clone", "-q", &format!("{url}/nope"), "nope"]).await;
    assert!(missing.contains("not found"), "{missing}");
}

#[tokio::test(flavor = "multi_thread")]
async fn git_needs_a_token_where_scn_does() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let auth = GuardConfig {
        required: true,
        auth: AuthConfig::parse("[token]\nissuer = \"https://scion.test\"\n").expect("auth"),
        signing_key_pem: Some(scion_api::scion_auth::token::generate_signing_key()),
    };
    let guard = Guard::build(auth.clone(), store.clone(), Arc::new(|_: &str| None)).expect("guard");
    guard
        .set_policy(
            "[[rule]]\nsubjects = [\"user:ada\"]\nrepos = [\"mono\"]\nactions = [\"land\"]\n\
             [[rule]]\nsubjects = [\"user:rita\"]\nrepos = [\"mono\"]\nactions = [\"read\"]\n\
             [[rule]]\nsubjects = [\"user:wes\"]\nrepos = [\"mono\"]\nactions = [\"write\"]\n",
        )
        .await
        .expect("policy");
    let token = |who: &'static str| {
        let g = guard.clone();
        async move { g.create_token("pat", who, vec![], "test", None).await.expect("token").0 }
    };
    let (ada, bob) = (token("ada").await, token("bob").await);
    let (rita, wes) = (token("rita").await, token("wes").await);
    let (url, _stop) = start(
        ServerConfig {
            auth,
            ..ServerConfig::default()
        },
        store.clone(),
    )
    .await;
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .auth(Auth::Bearer(ada.clone()))
        .build()
        .await
        .expect("connect");
    let a = c
        .changeset(None)
        .put("a.txt", "a\n")
        .message("a\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &a.id).await.expect("land");

    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let host = url.trim_start_matches("http://");
    // No credentials: git asks for them, and cannot here
    let e = fails(root, &["clone", "-q", &format!("{url}/mono"), "anon"]).await;
    assert!(
        e.contains("could not read Username") || e.contains("Authentication failed"),
        "{e}"
    );
    // A token that cannot read the repository, and a bad one
    let e = fails(root, &["clone", "-q", &format!("http://git:{bob}@{host}/mono"), "bob"]).await;
    assert!(e.contains("403") || e.contains("permission denied"), "{e}");
    let e = fails(
        root,
        &["clone", "-q", &format!("http://git:scn_pat_bad@{host}/mono"), "bad"],
    )
    .await;
    assert!(e.contains("Authentication failed") || e.contains("401"), "{e}");
    // The token as the password of any user name
    ok(root, &["clone", "-q", &format!("http://git:{ada}@{host}/mono"), "ada"]).await;
    assert_eq!(std::fs::read_to_string(root.join("ada/a.txt")).expect("read"), "a\n");

    // Pushing needs write, and moving a branch needs land
    let w = root.join("ada");
    std::fs::write(w.join("b.txt"), "b\n").expect("write");
    commit_all(&w, "b").await;
    let e = fails(&w, &["push", "-q", &format!("http://git:{rita}@{host}/mono"), "main"]).await;
    assert!(e.contains("403") || e.contains("permission denied"), "{e}");
    let e = fails(&w, &["push", "-q", &format!("http://git:{wes}@{host}/mono"), "main"]).await;
    assert!(e.contains("permission denied"), "{e}");
    ok(&w, &["push", "-q", "origin", "main"]).await;
    let sha = c.git_sha("main").await.expect("sha");
    assert_eq!(sha, Some(ok(&w, &["rev-parse", "HEAD"]).await.trim().to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_git_leaves_git_requests_unanswered() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let (url, _stop) = start(
        ServerConfig {
            git: false,
            ..ServerConfig::default()
        },
        store,
    )
    .await;
    let tmp = tempfile::tempdir().expect("tmp");
    fails(tmp.path(), &["ls-remote", &format!("{url}/mono")]).await;
    // gRPC still works
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .build()
        .await
        .expect("connect");
    let a = c.changeset(None).put("a", "a").message("a").create().await;
    assert!(a.is_ok(), "{a:?}");
}

/// Commits imported from git keep their SHAs through the gateway: the clone has exactly the
/// source repository's objects, merges included.
#[tokio::test(flavor = "multi_thread")]
async fn imported_history_comes_back_object_for_object() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let src = root.join("src");
    std::fs::create_dir(&src).expect("mkdir");
    let commit = |msg: &'static str, date: &'static str| {
        let src = src.clone();
        async move {
            ok(&src, &["add", "-A"]).await;
            let o = git(
                &src,
                &[
                    "-c",
                    "user.name=Ada",
                    "-c",
                    "user.email=ada@x",
                    "commit",
                    "-qm",
                    msg,
                    &format!("--date={date}"),
                ],
            )
            .await;
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
    };
    ok(&src, &["init", "-q", "-b", "main"]).await;
    std::fs::create_dir_all(src.join("a/b")).expect("mkdir");
    std::fs::write(src.join("a/b/c.txt"), "c\n").expect("write");
    std::fs::write(src.join("top.txt"), "top\n").expect("write");
    commit("one", "2020-01-01T00:00:00+0800").await;
    ok(&src, &["checkout", "-qb", "side"]).await;
    std::fs::write(src.join("a/side.txt"), "side\n").expect("write");
    commit("side", "2020-01-02T00:00:00+0800").await;
    ok(&src, &["checkout", "-q", "main"]).await;
    std::fs::write(src.join("top.txt"), "top 2\n").expect("write");
    commit("two", "2020-01-03T00:00:00-0130").await;
    let o = git(
        &src,
        &[
            "-c",
            "user.name=Ada",
            "-c",
            "user.email=ada@x",
            "merge",
            "-q",
            "--no-edit",
            "side",
        ],
    )
    .await;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // A commit in another encoding, then a signed one (written by hand: no gpg needed)
    std::fs::write(src.join("top.txt"), "top 3\n").expect("write");
    ok(&src, &["add", "-A"]).await;
    ok(
        &src,
        &[
            "-c",
            "user.name=Ada",
            "-c",
            "user.email=ada@x",
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "-qm",
            "latin",
        ],
    )
    .await;
    let head = ok(&src, &["cat-file", "commit", "HEAD"]).await;
    let parent = ok(&src, &["rev-parse", "HEAD"]).await;
    let (headers, _) = head.split_once("\n\n").expect("commit");
    let tree = headers.lines().next().expect("tree line");
    let signed = format!(
        "{tree}\nparent {}\nauthor Ada <ada@x> 1600000000 +0000\ncommitter Ada <ada@x> 1600000000 +0000\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nsigned\n",
        parent.trim()
    );
    std::fs::write(root.join("signed.txt"), signed).expect("write");
    let sha = ok(
        &src,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            root.join("signed.txt").to_str().expect("path"),
        ],
    )
    .await;
    ok(&src, &["update-ref", "refs/heads/main", sha.trim()]).await;

    let store = Arc::new(MemStore::new());
    store.create_repo("imported").await.expect("repo");
    let (url, _stop) = start(ServerConfig::default(), store).await;
    let c = Client::builder()
        .endpoint(&url)
        .repo("imported")
        .build()
        .await
        .expect("connect");
    scion_gitimport::import(&c, &src, "main", "main", |_, _| {})
        .await
        .expect("import");

    ok(root, &["clone", "-q", "--mirror", &format!("{url}/imported"), "copy"]).await;
    let objects =
        |dir: std::path::PathBuf| async move { ok(&dir, &["cat-file", "--batch-all-objects", "--batch-check"]).await };
    assert_eq!(objects(root.join("copy")).await, objects(src.join(".git")).await);
    assert_eq!(
        ok(&root.join("copy"), &["rev-parse", "main"]).await,
        ok(&src, &["rev-parse", "main"]).await
    );
}

/// A clone with unrelated history sends haves the server does not know, round after round.
#[tokio::test(flavor = "multi_thread")]
async fn fetching_into_unrelated_history_negotiates_then_sends_everything() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let (url, _stop) = start(ServerConfig::default(), store).await;
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .build()
        .await
        .expect("connect");
    let a = c
        .changeset(None)
        .put("a.txt", "a\n")
        .message("a\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &a.id).await.expect("land");

    let tmp = tempfile::tempdir().expect("tmp");
    let w = tmp.path().join("w");
    std::fs::create_dir(&w).expect("mkdir");
    ok(&w, &["init", "-q", "-b", "local"]).await;
    for i in 0..40 {
        ok(
            &w,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@x",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("{i}"),
            ],
        )
        .await;
    }
    for version in ["2", "0"] {
        ok(
            &w,
            &[
                "-c",
                &format!("protocol.version={version}"),
                "fetch",
                "-q",
                &format!("{url}/mono"),
                &format!("main:v{version}"),
            ],
        )
        .await;
        assert_eq!(ok(&w, &["show", &format!("v{version}:a.txt")]).await, "a\n");
    }
}

/// git commit with fixed identities.
async fn commit_all(dir: &Path, msg: &str) {
    ok(dir, &["add", "-A"]).await;
    ok(
        dir,
        &[
            "-c",
            "user.name=Grace",
            "-c",
            "user.email=grace@x",
            "commit",
            "-qm",
            msg,
        ],
    )
    .await;
}

/// Pushes from git land as Scion commits that keep their SHAs: files, executables, symlinks,
/// renames, file/directory swaps, merges, signed commits, branches and tags; non-fast-forwards
/// are refused, forced pushes and deletes work, and annotated tags are refused.
#[tokio::test(flavor = "multi_thread")]
async fn git_pushes_land_as_scion_commits() {
    if !have_git() {
        eprintln!("git not installed; skipping");
        return;
    }
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let (url, _stop) = start(ServerConfig::default(), store.clone()).await;
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .build()
        .await
        .expect("connect");
    let a = c
        .changeset(None)
        .put("README.md", "hello\n")
        .put("lib/a.rs", "a\n")
        .put("lib/b.rs", "b\n")
        .put("x", "x is a file\n")
        .message("base\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &a.id).await.expect("land");

    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    ok(root, &["clone", "-q", &format!("{url}/mono"), "w"]).await;
    let w = root.join("w");
    // Edits, a new directory, an executable, a symlink, a rename, a deletion, file <-> directory
    std::fs::write(w.join("README.md"), "hello from git\n").expect("write");
    std::fs::create_dir_all(w.join("docs/deep")).expect("mkdir");
    std::fs::write(w.join("docs/deep/guide.md"), "# Guide\n").expect("write");
    std::fs::write(w.join("run.sh"), "#!/bin/sh\n").expect("write");
    ok(&w, &["update-index", "--add", "--chmod=+x", "run.sh"]).await;
    std::os::unix::fs::symlink("README.md", w.join("link")).expect("symlink");
    ok(&w, &["mv", "lib/b.rs", "lib/c.rs"]).await;
    std::fs::remove_file(w.join("lib/a.rs")).expect("rm");
    std::fs::remove_file(w.join("x")).expect("rm");
    std::fs::create_dir(w.join("x")).expect("mkdir");
    std::fs::write(w.join("x/inside.txt"), "x is a directory now\n").expect("write");
    commit_all(&w, "many changes").await;
    // Renaming a whole directory: git sends no tree or blob for it
    ok(&w, &["mv", "docs", "manual"]).await;
    commit_all(&w, "rename docs").await;
    ok(&w, &["push", "-q", "origin", "main"]).await;
    let head = ok(&w, &["rev-parse", "HEAD"]).await;
    assert_eq!(c.git_sha("main").await.expect("sha").as_deref(), Some(head.trim()));
    let read = |path: &'static str| {
        let c = c.clone();
        async move {
            c.read_file("main", path)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        }
    };
    assert_eq!(read("README.md").await.as_deref(), Some("hello from git\n"));
    assert_eq!(read("manual/deep/guide.md").await.as_deref(), Some("# Guide\n"));
    assert_eq!(read("lib/c.rs").await.as_deref(), Some("b\n"));
    assert_eq!(read("x/inside.txt").await.as_deref(), Some("x is a directory now\n"));
    assert_eq!(read("lib/a.rs").await, None);

    // A merge, and a branch and a tag
    ok(&w, &["checkout", "-qb", "side"]).await;
    std::fs::write(w.join("side.txt"), "side\n").expect("write");
    commit_all(&w, "side").await;
    ok(&w, &["checkout", "-q", "main"]).await;
    std::fs::write(w.join("main.txt"), "main\n").expect("write");
    commit_all(&w, "main").await;
    ok(
        &w,
        &[
            "-c",
            "user.name=Grace",
            "-c",
            "user.email=grace@x",
            "merge",
            "-q",
            "--no-edit",
            "side",
        ],
    )
    .await;
    ok(&w, &["tag", "v1"]).await;
    ok(&w, &["push", "-q", "origin", "main", "side", "v1"]).await;
    let refs = ok(root, &["ls-remote", &format!("{url}/mono")]).await;
    let local = |r: &'static str| {
        let w = w.clone();
        async move { ok(&w, &["rev-parse", r]).await.trim().to_string() }
    };
    for (git_ref, rev) in [
        ("refs/heads/main", "main"),
        ("refs/heads/side", "side"),
        ("refs/tags/v1", "v1"),
    ] {
        assert!(
            refs.contains(&format!("{}\t{git_ref}", local(rev).await)),
            "{git_ref}: {refs}"
        );
    }
    // A fresh clone has exactly the pushed history
    ok(root, &["clone", "-q", "--mirror", &format!("{url}/mono"), "m"]).await;
    let m = root.join("m");
    ok(&m, &["fsck", "--strict", "--no-dangling"]).await;
    assert_eq!(
        ok(&m, &["log", "--format=%H %P %an %s", "main"]).await,
        ok(&w, &["log", "--format=%H %P %an %s", "main"]).await
    );

    // Someone else lands first: a plain push is refused, a forced one wins
    let b = c
        .changeset(Some(
            c.resolve_ref("main").await.expect("main").expect("main").changeset,
        ))
        .put("other.txt", "other\n")
        .message("from scn\n")
        .create()
        .await
        .expect("commit");
    c.land("main", &b.id).await.expect("land");
    std::fs::write(w.join("late.txt"), "late\n").expect("write");
    commit_all(&w, "late").await;
    let e = fails(&w, &["push", "-q", "origin", "main"]).await;
    assert!(e.contains("rejected") || e.contains("fetch first"), "{e}");
    ok(&w, &["pull", "-q", "--rebase", "origin", "main"]).await;
    ok(&w, &["push", "-q", "origin", "main"]).await;
    assert_eq!(read("other.txt").await.as_deref(), Some("other\n"));
    assert_eq!(read("late.txt").await.as_deref(), Some("late\n"));
    ok(&w, &["reset", "-q", "--hard", "HEAD~2"]).await;
    ok(&w, &["push", "-q", "--force", "origin", "main"]).await;
    assert_eq!(read("late.txt").await, None);
    assert_eq!(c.git_sha("main").await.expect("sha"), Some(local("HEAD").await));

    // Deleting a branch; an annotated tag; a ref outside heads and tags
    ok(&w, &["push", "-q", "origin", ":side"]).await;
    assert!(
        !ok(root, &["ls-remote", &format!("{url}/mono")])
            .await
            .contains("refs/heads/side")
    );
    ok(
        &w,
        &[
            "-c",
            "user.name=Grace",
            "-c",
            "user.email=grace@x",
            "tag",
            "-a",
            "-m",
            "annotated",
            "v2",
        ],
    )
    .await;
    let e = fails(&w, &["push", "origin", "v2"]).await;
    assert!(e.contains("annotated tags are not supported"), "{e}");
    let e = fails(&w, &["push", "origin", "HEAD:refs/notes/x"]).await;
    assert!(e.contains("only refs/heads/* and refs/tags/* can be pushed"), "{e}");

    // A signed commit keeps its SHA
    let raw = ok(&w, &["cat-file", "commit", "HEAD"]).await;
    let (headers, _) = raw.split_once("\n\n").expect("commit");
    let tree = headers.lines().next().expect("tree");
    let signed = format!(
        "{tree}\nparent {}\nauthor Grace <grace@x> 1600000000 +0000\ncommitter Grace <grace@x> 1600000000 +0000\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nsigned\n",
        local("HEAD").await
    );
    std::fs::write(root.join("signed.txt"), signed).expect("write");
    let sha = ok(
        &w,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            root.join("signed.txt").to_str().expect("path"),
        ],
    )
    .await;
    ok(
        &w,
        &["push", "-q", "origin", &format!("{}:refs/heads/main", sha.trim())],
    )
    .await;
    assert_eq!(c.git_sha("main").await.expect("sha").as_deref(), Some(sha.trim()));
    ok(&m, &["fetch", "-q", "origin", "+refs/heads/*:refs/heads/*"]).await;
    assert_eq!(ok(&m, &["rev-parse", "main"]).await.trim(), sha.trim());
    ok(&m, &["fsck", "--strict", "--no-dangling"]).await;
}
