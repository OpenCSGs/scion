//! 各子命令的实现。每个命令返回可渲染为文本或 JSON 的结果。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use scion_client::{Client, EntryMode};
use scion_wc::{Progress, Sparse, Status, SyncOutcome, WorkingCopy};
use serde::Serialize;

use crate::output::{CliError, DoctorOut, Render, doctor_out};
use crate::progress::Meter;

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:7080";
const DEFAULT_REF: &str = "main";

/// 命令行给出的全局选项。
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub endpoint: Option<String>,
    pub repo: Option<String>,
    pub cwd: PathBuf,
    /// Print progress and git-style notes on standard error (not with `--json` or `--quiet`).
    pub chatty: bool,
}

impl Ctx {
    /// 当前目录所在的工作副本（如果有）。
    fn working_copy(&self) -> Option<WorkingCopy> {
        WorkingCopy::open(&self.cwd).ok()
    }

    pub fn has_working_copy(&self) -> bool {
        self.working_copy().is_some()
    }

    /// Expands git's `HEAD` / `@` (alone or followed by `~N` / `^N`, e.g. `HEAD~1`) to the
    /// working copy's current commit: the pending commit if any, else the synced one. Other
    /// revisions, including suffixes on refs and ids, are resolved by the server.
    pub fn expand_rev(&self, rev: &str) -> Result<String, CliError> {
        let rest = match rev.strip_prefix("HEAD").or_else(|| rev.strip_prefix('@')) {
            Some(r) if r.is_empty() || r.starts_with(['~', '^']) => r,
            _ => return Ok(rev.to_owned()),
        };
        let wc = self
            .working_copy()
            .ok_or_else(|| CliError::Usage(format!("{rev}: HEAD needs a working copy")))?;
        let current = wc
            .state()
            .current()?
            .ok_or_else(|| CliError::Usage(format!("{rev}: the working copy has no commits yet")))?;
        Ok(format!("{current}{rest}"))
    }

    /// 服务端地址：命令行或环境变量 > 当前工作副本记录 > 默认值。
    pub fn endpoint_or_default(&self) -> String {
        self.endpoint_or(self.working_copy().as_ref())
    }

    /// 服务端地址：命令行或环境变量 > 工作副本记录 > 默认值。
    fn endpoint_or(&self, wc: Option<&WorkingCopy>) -> String {
        self.endpoint
            .clone()
            .or_else(|| wc.map(|w| w.state().endpoint.clone()))
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned())
    }
}

/// Splits a repository URL, `http(s)://host[:port]/REPO`, into the server's address and the
/// repository name (empty when the URL names only the server). Anything else is `None`: a plain
/// repository name.
pub fn split_repo_url(s: &str) -> Result<Option<(String, String)>, CliError> {
    let Some(scheme_end) = ["http://", "https://"]
        .iter()
        .find_map(|p| s.starts_with(p).then_some(p.len()))
    else {
        return Ok(None);
    };
    let (host, path) = s[scheme_end..].split_once('/').unwrap_or((&s[scheme_end..], ""));
    if host.is_empty() || host.contains('@') || s.contains(['?', '#']) {
        return Err(CliError::Usage(format!(
            "{s}: expected http(s)://host[:port]/REPOSITORY"
        )));
    }
    let repo = path.trim_matches('/');
    if repo.split('/').any(str::is_empty) && !repo.is_empty() {
        return Err(CliError::Usage(format!("{s}: empty segment in the repository name")));
    }
    Ok(Some((s[..scheme_end + host.len()].to_owned(), repo.to_owned())))
}

impl Ctx {
    /// A repository argument that may be a URL: the server's address in it replaces
    /// `--endpoint` / `SCION_ENDPOINT` for this command, and the repository name is returned.
    pub fn repo_arg(&mut self, arg: &str) -> Result<String, CliError> {
        match split_repo_url(arg)? {
            Some((endpoint, repo)) => {
                self.endpoint = Some(endpoint);
                Ok(repo)
            }
            None => Ok(arg.to_owned()),
        }
    }

    /// Like [`Ctx::repo_arg`], for arguments that must name a repository.
    pub fn named_repo_arg(&mut self, arg: &str) -> Result<String, CliError> {
        let repo = self.repo_arg(arg)?;
        if repo.is_empty() {
            return Err(CliError::Usage(format!("{arg}: the URL names no repository")));
        }
        Ok(repo)
    }

    /// `repo fork SOURCE TARGET`: either may be a URL, and URLs must name the same server,
    /// since a fork stays on one server.
    pub fn fork_args(&mut self, source: &str, target: &str) -> Result<(String, String), CliError> {
        if let (Some((a, _)), Some((b, _))) = (split_repo_url(source)?, split_repo_url(target)?)
            && a != b
        {
            return Err(CliError::Usage(format!(
                "a fork stays on one server: {a} and {b} differ"
            )));
        }
        Ok((self.named_repo_arg(source)?, self.named_repo_arg(target)?))
    }

    /// A server given as an argument (`scn login URL`): its address, without a path.
    pub fn server_arg(&mut self, arg: &str) -> Result<(), CliError> {
        match split_repo_url(arg)? {
            Some((endpoint, _)) => {
                self.endpoint = Some(endpoint);
                Ok(())
            }
            None => Err(CliError::Usage(format!("{arg}: expected http(s)://host[:port]"))),
        }
    }
}

/// 连接服务端：默认经本机 daemon（必要时拉起它），daemon 不可用时直连。
pub async fn connect(endpoint: &str, repo: Option<&str>) -> Result<Client, CliError> {
    let mut b = Client::builder()
        .endpoint(endpoint)
        .auth(crate::login::auth_for(endpoint, |k| std::env::var(k).ok()))
        .client_name("scn")
        .tls(scion_client::Tls::from_env()?)
        .connect_timeout(Duration::from_secs(5));
    if let Some(r) = repo {
        b = b.repo(r);
    }
    let env = |k: &str| std::env::var(k).ok();
    if crate::daemon::enabled(env) {
        let socket = crate::daemon::config(env).socket;
        if let Some(c) = crate::daemon::connect(b.clone(), &socket).await {
            return Ok(c);
        }
    }
    Ok(b.build().await?)
}

/// 打开当前工作副本并连接到它记录的服务端。
async fn open_wc(ctx: &Ctx) -> Result<(WorkingCopy, Client), CliError> {
    let wc = WorkingCopy::open(&ctx.cwd)?;
    let client = connect(&ctx.endpoint_or(Some(&wc)), Some(&wc.state().repo)).await?;
    Ok((wc, client))
}

pub async fn doctor(ctx: &Ctx) -> Result<DoctorOut, CliError> {
    let endpoint = ctx.endpoint_or(ctx.working_copy().as_ref());
    let client = connect(&endpoint, None).await?;
    let mut out = doctor_out(&endpoint, client.server_info());
    out.objects = if client.via_daemon() {
        format!(
            "daemon ({})",
            crate::daemon::config(|k| std::env::var(k).ok()).socket.display()
        )
    } else {
        client.object_endpoint().to_owned()
    };
    Ok(out)
}

/// 解析 `Name <email>`。
pub fn parse_author(s: &str) -> Result<(String, String), CliError> {
    let bad = || CliError::Usage(format!("invalid author {s:?}; expected \"Name <email>\""));
    let (name, rest) = s.split_once('<').ok_or_else(bad)?;
    let email = rest.strip_suffix('>').ok_or_else(bad)?.trim();
    let name = name.trim();
    if name.is_empty() || email.is_empty() {
        return Err(bad());
    }
    Ok((name.to_owned(), email.to_owned()))
}

/// A git-style note on standard error, unless `--json` or `--quiet`.
pub fn note(ctx: &Ctx, msg: &str) {
    if ctx.chatty {
        eprintln!("{msg}");
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

// ---------- clone ----------

#[derive(Debug, Serialize)]
pub struct CloneOut {
    pub schema: &'static str,
    pub repo: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub base: Option<String>,
    /// 稀疏检出的目录；全部检出时省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse: Option<Vec<String>>,
}

/// Like `git clone`, nothing on standard output: progress and notes go to standard error.
impl Render for CloneOut {
    fn text(&self) -> String {
        String::new()
    }
}

/// 默认目录名：仓库名的最后一段。
pub fn default_dir(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

pub async fn clone(
    ctx: &Ctx,
    repo: &str,
    dir: Option<&Path>,
    reference: &str,
    sparse: Option<&[String]>,
) -> Result<CloneOut, CliError> {
    let sparse = match sparse {
        Some(dirs) => Sparse::dirs(dirs)?,
        None => Sparse::full(),
    };
    let endpoint = ctx.endpoint.clone().unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
    let client = connect(&endpoint, Some(repo)).await?;
    let dir = dir.unwrap_or_else(|| Path::new(default_dir(repo)));
    let target = ctx.cwd.join(dir);
    note(ctx, &format!("Cloning into '{}'...", dir.display()));
    let meter = Meter::stderr(ctx.chatty);
    let wc = WorkingCopy::clone_with_progress(&client, &endpoint, repo, reference, &target, sparse, &|p| match p {
        Progress::Listing { files } => meter.update("Listing files", files as u64, None, None),
        Progress::Checkout { done, total, bytes } => {
            meter.update("Receiving objects", done as u64, Some(total as u64), Some(bytes));
        }
    })
    .await?;
    meter.done();
    if wc.state().base.is_none() {
        note(
            ctx,
            &format!("warning: You appear to have cloned an empty repository ({reference} does not exist yet)."),
        );
    }
    Ok(CloneOut {
        schema: "scion.cli.clone/v1",
        repo: repo.to_owned(),
        path: target.display().to_string(),
        reference: reference.to_owned(),
        base: wc.state().base.clone(),
        sparse: wc.state().sparse.clone(),
    })
}

// ---------- sparse ----------

#[derive(Debug, Serialize)]
pub struct SparseOut {
    pub schema: &'static str,
    /// 范围内的目录；全部检出时为 null。
    pub dirs: Option<Vec<String>>,
    pub written: usize,
    pub removed: usize,
}

impl Render for SparseOut {
    fn text(&self) -> String {
        let mut s = match &self.dirs {
            None => "full checkout".to_owned(),
            Some(d) if d.is_empty() => "sparse checkout: root files only".to_owned(),
            Some(d) => format!("sparse checkout:\n  {}", d.join("\n  ")),
        };
        if self.written + self.removed > 0 {
            s.push_str(&format!("\n({} written, {} removed)", self.written, self.removed));
        }
        s
    }
}

pub async fn sparse(ctx: &Ctx, cmd: crate::SparseCmd) -> Result<SparseOut, CliError> {
    use crate::SparseCmd;
    let mut wc = WorkingCopy::open(&ctx.cwd)?;
    let current = wc.sparse()?;
    let target = match cmd {
        SparseCmd::Ls => None,
        SparseCmd::Set { dirs } => Some(Sparse::dirs(&dirs)?),
        SparseCmd::Add { dirs } => Some(current.add(&dirs)?),
        SparseCmd::Rm { dirs } => Some(current.remove(&dirs)?),
        SparseCmd::Disable => Some(Sparse::full()),
    };
    let (written, removed) = match target {
        Some(t) if t != current => {
            let client = connect(&ctx.endpoint_or(Some(&wc)), Some(&wc.state().repo)).await?;
            wc.set_sparse(&client, t).await?
        }
        _ => (0, 0),
    };
    Ok(SparseOut {
        schema: "scion.cli.sparse/v1",
        dirs: wc.state().sparse.clone(),
        written,
        removed,
    })
}

// ---------- status ----------

#[derive(Debug, Clone, Serialize)]
pub struct StatusOut {
    pub schema: &'static str,
    #[serde(rename = "ref")]
    pub reference: String,
    pub base: Option<String>,
    /// 已提交未合入的提交。
    pub pending: Option<String>,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    /// Paths with unresolved conflicts (see `scn resolve`).
    pub conflicted: Vec<String>,
    /// Conflict kind of each conflicted path.
    #[serde(skip)]
    pub kinds: std::collections::BTreeMap<String, String>,
    /// Finishing a merge (true) or a sync (false) that stopped on conflicts.
    #[serde(skip)]
    pub merging: Option<bool>,
    /// `scn status -s`.
    #[serde(skip)]
    pub short: bool,
    /// The current directory inside the working copy (`/`-separated, empty at the root): text
    /// output shows paths relative to it, like git.
    #[serde(skip)]
    pub cwd: String,
}

/// `path` (relative to the working copy root) as seen from `cwd` (also relative to the root).
fn relative_to(cwd: &str, path: &str) -> String {
    let (mut from, mut to): (Vec<&str>, Vec<&str>) = (
        cwd.split('/').filter(|s| !s.is_empty()).collect(),
        path.split('/').collect(),
    );
    while !from.is_empty() && to.len() > 1 && from[0] == to[0] {
        from.remove(0);
        to.remove(0);
    }
    let mut parts = vec![".."; from.len()];
    parts.extend(to);
    parts.join("/")
}

/// git's label and `--short` code for a conflict kind.
fn conflict_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "add_add" | "path" => ("both added", "AA"),
        "modify_delete" => ("deleted by them", "UD"),
        "delete_modify" => ("deleted by us", "DU"),
        _ => ("both modified", "UU"),
    }
}

/// `git status`'s long format, or with `-s` its short one (`XY PATH`; scn has no staging area,
/// so every change shows as staged).
impl Render for StatusOut {
    fn text(&self) -> String {
        let unmerged: Vec<(String, &str, &str)> = self
            .conflicted
            .iter()
            .map(|p| {
                let (label, code) = conflict_label(self.kinds.get(p).map_or("content", String::as_str));
                (relative_to(&self.cwd, p), label, code)
            })
            .collect();
        let is_unmerged = |p: &String| self.conflicted.contains(p);
        let changes: Vec<(&str, &str, String)> = [
            ("new file:", "A ", &self.added),
            ("modified:", "M ", &self.modified),
            ("deleted:", "D ", &self.deleted),
        ]
        .into_iter()
        .flat_map(|(label, code, paths)| {
            paths
                .iter()
                .filter(|p| !is_unmerged(p))
                .map(move |p| (label, code, relative_to(&self.cwd, p)))
        })
        .collect();
        if self.short {
            let mut lines: Vec<String> = unmerged.iter().map(|(p, _, code)| format!("{code} {p}")).collect();
            lines.extend(changes.iter().map(|(_, code, p)| format!("{code} {p}")));
            return lines.join("\n");
        }
        let mut head = vec![format!("On branch {}", self.reference)];
        match (&self.pending, &self.base) {
            (Some(p), _) => head.push(format!(
                "Your commit {} is not landed yet.\n  (use \"scn land\" to publish your commit)",
                short(p)
            )),
            (None, None) => head.push("\nNo commits yet".to_owned()),
            _ => {}
        }
        match (self.merging, unmerged.is_empty()) {
            (Some(true), false) => head.push(
                "You have unmerged paths.\n  (fix conflicts and run \"scn commit\")\n  \
                 (use \"scn merge --abort\" to abort the merge)"
                    .to_owned(),
            ),
            (Some(true), true) => head.push(
                "All conflicts fixed but you are still merging.\n  (use \"scn commit\" to conclude merge)".to_owned(),
            ),
            (Some(false), false) => head.push(format!(
                "You are currently rebasing your commit onto {}.\n  (fix conflicts and then run \"scn commit\")",
                self.reference
            )),
            (Some(false), true) => head.push(format!(
                "You are currently rebasing your commit onto {}.\n  (all conflicts fixed: run \"scn commit\")",
                self.reference
            )),
            (None, _) => {}
        }
        let mut blocks = vec![head.join("\n")];
        if !changes.is_empty() {
            let mut b = "Changes to be committed:\n  (use \"scn restore <file>...\" to discard changes)".to_owned();
            for (label, _, p) in &changes {
                b.push_str(&format!("\n\t{label:<12}{p}"));
            }
            blocks.push(b);
        }
        if !unmerged.is_empty() {
            let mut b = "Unmerged paths:\n  (use \"scn resolve <file>...\" to mark resolution)".to_owned();
            for (p, label, _) in &unmerged {
                b.push_str(&format!("\n\t{:<17}{p}", format!("{label}:")));
            }
            blocks.push(b);
        }
        if changes.is_empty() && unmerged.is_empty() {
            blocks.push("nothing to commit, working tree clean".to_owned());
        }
        blocks.join("\n\n")
    }
}

pub fn status_out(wc: &WorkingCopy, s: Status) -> StatusOut {
    let conflict = wc.state().conflict.as_ref();
    StatusOut {
        schema: "scion.cli.status/v1",
        reference: wc.state().reference.clone(),
        base: wc.state().base.clone(),
        pending: wc.state().head.clone(),
        added: s.added,
        modified: s.modified,
        deleted: s.deleted,
        conflicted: s.conflicted,
        kinds: conflict
            .map(|c| {
                c.paths
                    .iter()
                    .map(|(p, sides)| (p.clone(), sides.kind.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        merging: conflict.map(|c| c.merge.is_some()),
        short: false,
        cwd: String::new(),
    }
}

pub fn status(ctx: &Ctx, short: bool) -> Result<StatusOut, CliError> {
    let wc = WorkingCopy::open(&ctx.cwd)?;
    let s = wc.status()?;
    let cwd = wc_relative(wc.root(), &ctx.cwd, &[".".to_owned()])?.remove(0);
    Ok(StatusOut {
        short,
        cwd: if cwd == "." { String::new() } else { cwd },
        ..status_out(&wc, s)
    })
}

// ---------- conflicts ----------

#[derive(Debug, Serialize)]
pub struct ResolveOut {
    pub schema: &'static str,
    pub resolved: Vec<String>,
    /// Paths still unresolved.
    pub remaining: Vec<String>,
}

impl Render for ResolveOut {
    fn text(&self) -> String {
        if self.remaining.is_empty() {
            "all conflicts resolved; run `scn commit`".to_owned()
        } else {
            format!("still conflicted: {}", self.remaining.join(", "))
        }
    }
}

/// `scn resolve`: paths are relative to the current directory; none lists the conflicts.
pub async fn resolve(
    ctx: &Ctx,
    paths: &[String],
    side: Option<scion_wc::Side>,
    force: bool,
) -> Result<ResolveOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    if paths.is_empty() {
        let remaining: Vec<String> = match &wc.state().conflict {
            Some(c) => c.paths.keys().cloned().collect(),
            None => Vec::new(),
        };
        return Ok(ResolveOut {
            schema: "scion.cli.resolve/v1",
            resolved: Vec::new(),
            remaining,
        });
    }
    let rel = wc_relative(wc.root(), &ctx.cwd, paths)?;
    let before: Vec<String> = match &wc.state().conflict {
        Some(c) => c.paths.keys().cloned().collect(),
        None => Vec::new(),
    };
    let remaining = wc.resolve(&client, &rel, side, force).await?;
    Ok(ResolveOut {
        schema: "scion.cli.resolve/v1",
        resolved: before.into_iter().filter(|p| !remaining.contains(p)).collect(),
        remaining,
    })
}

#[derive(Debug, Serialize)]
pub struct AbortOut {
    pub schema: &'static str,
}

impl Render for AbortOut {
    fn text(&self) -> String {
        "merge aborted; the working copy is back where it was".to_owned()
    }
}

pub async fn abort_merge(ctx: &Ctx) -> Result<AbortOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    wc.abort_merge(&client).await?;
    Ok(AbortOut {
        schema: "scion.cli.merge-abort/v1",
    })
}

// ---------- commit / land / sync ----------

#[derive(Debug, Serialize)]
pub struct CommitOut {
    pub schema: &'static str,
    /// 没有改动时为 None。
    pub changeset: Option<String>,
    pub generation: Option<u64>,
    /// For the text: the branch, the subject, whether it is the branch's first commit, and the files.
    #[serde(skip)]
    pub summary: Option<CommitSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct CommitSummary {
    pub reference: String,
    pub subject: String,
    pub root: bool,
    pub files: Vec<scion_wc::FileStat>,
}

fn git_mode(m: scion_wc::Mode) -> &'static str {
    match m {
        scion_wc::Mode::File => "100644",
        scion_wc::Mode::Executable => "100755",
        scion_wc::Mode::Symlink => "120000",
    }
}

/// Like git: `[main abc123456789] subject`, ` 2 files changed`, then created, deleted and
/// mode-changed files. (No line counts: they would mean downloading every changed file's old
/// version.)
impl Render for CommitOut {
    fn text(&self) -> String {
        let (Some(c), Some(sum)) = (&self.changeset, &self.summary) else {
            return "nothing to commit, working tree clean".to_owned();
        };
        let root = if sum.root { " (root-commit)" } else { "" };
        let n = sum.files.len();
        let mut s = format!(
            "[{}{root} {}] {}\n {n} file{} changed",
            sum.reference,
            short(c),
            sum.subject,
            if n == 1 { "" } else { "s" }
        );
        for f in &sum.files {
            match (f.old_mode, f.new_mode) {
                (None, Some(m)) => s.push_str(&format!("\n create mode {} {}", git_mode(m), f.path)),
                (Some(m), None) => s.push_str(&format!("\n delete mode {} {}", git_mode(m), f.path)),
                (Some(a), Some(b)) if a != b => {
                    s.push_str(&format!("\n mode change {} => {} {}", git_mode(a), git_mode(b), f.path));
                }
                _ => {}
            }
        }
        s
    }
}

fn commit_out(
    wc: &WorkingCopy,
    root: bool,
    created: Option<(scion_client::CreatedChangeset, Vec<scion_wc::FileStat>)>,
) -> CommitOut {
    let subject = wc
        .state()
        .head_message
        .as_deref()
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("");
    CommitOut {
        schema: "scion.cli.commit/v1",
        changeset: created.as_ref().map(|(c, _)| c.id.to_string()),
        generation: created.as_ref().map(|(c, _)| c.generation),
        summary: created.map(|(_, files)| CommitSummary {
            reference: wc.state().reference.clone(),
            subject: subject.to_owned(),
            root,
            files,
        }),
    }
}

/// `scn commit --amend`: replaces the pending commit. Unlike a new commit, the default author is
/// the old commit's author, not the configured one.
pub async fn amend(ctx: &Ctx, message: Option<&str>, author: Option<&str>) -> Result<CommitOut, CliError> {
    let author = author.map(parse_author).transpose()?;
    let (mut wc, client) = open_wc(ctx).await?;
    let root = wc.state().base.is_none();
    let created = wc
        .amend_with_stat(&client, message, author.as_ref().map(|(n, e)| (n.as_str(), e.as_str())))
        .await?;
    Ok(commit_out(&wc, root, created))
}

#[derive(Debug, Serialize)]
pub struct UncommitOut {
    pub schema: &'static str,
    /// The dropped commit.
    pub changeset: String,
}

impl Render for UncommitOut {
    fn text(&self) -> String {
        format!(
            "uncommitted {}; its changes are now uncommitted local changes",
            short(&self.changeset)
        )
    }
}

pub async fn uncommit(ctx: &Ctx) -> Result<UncommitOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let id = wc.uncommit(&client).await?;
    Ok(UncommitOut {
        schema: "scion.cli.uncommit/v1",
        changeset: id.to_string(),
    })
}

#[derive(Debug, Serialize)]
pub struct RestoreOut {
    pub schema: &'static str,
    pub restored: Vec<String>,
    /// Added files under the given paths; restore leaves them alone.
    pub kept_added: Vec<String>,
}

impl Render for RestoreOut {
    fn text(&self) -> String {
        let mut lines: Vec<String> = self.restored.iter().map(|p| format!("restored {p}")).collect();
        if lines.is_empty() {
            lines.push("nothing to restore".to_owned());
        }
        if !self.kept_added.is_empty() {
            lines.push(format!(
                "kept {} added file(s) (restore never deletes files): {}",
                self.kept_added.len(),
                self.kept_added.join(", ")
            ));
        }
        lines.join("\n")
    }
}

/// Paths are relative to the current directory, like other commands that take local paths.
pub async fn restore(ctx: &Ctx, paths: &[String]) -> Result<RestoreOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let rel = wc_relative(wc.root(), &ctx.cwd, paths)?;
    let r = wc.restore(&client, &rel).await?;
    Ok(RestoreOut {
        schema: "scion.cli.restore/v1",
        restored: r.restored,
        kept_added: r.kept_added,
    })
}

/// Converts paths given relative to `cwd` into working-copy paths (`/`-separated, no `..`).
fn wc_relative(root: &Path, cwd: &Path, paths: &[String]) -> Result<Vec<String>, CliError> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let base = cwd
        .strip_prefix(&root)
        .map_err(|_| CliError::Usage(format!("{} is outside the working copy", cwd.display())))?;
    paths
        .iter()
        .map(|p| {
            let mut parts: Vec<String> = base
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            for part in p.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        parts
                            .pop()
                            .ok_or_else(|| CliError::Usage(format!("{p:?} is outside the working copy")))?;
                    }
                    other => parts.push(other.to_owned()),
                }
            }
            Ok(if parts.is_empty() {
                ".".to_owned()
            } else {
                parts.join("/")
            })
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct SwitchOut {
    pub schema: &'static str,
    #[serde(rename = "ref")]
    pub reference: String,
    pub created: bool,
    pub tip: Option<String>,
    pub written: usize,
    pub removed: usize,
    /// Already on that branch.
    #[serde(skip)]
    pub already: bool,
}

/// Like `git switch`.
impl Render for SwitchOut {
    fn text(&self) -> String {
        if self.created {
            format!("Switched to a new branch '{}'", self.reference)
        } else if self.already {
            format!("Already on '{}'", self.reference)
        } else {
            format!("Switched to branch '{}'", self.reference)
        }
    }
}

pub async fn switch(ctx: &Ctx, reference: &str, create: bool) -> Result<SwitchOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let already = wc.state().reference == reference;
    let s = wc.switch(&client, reference, create).await?;
    Ok(SwitchOut {
        schema: "scion.cli.switch/v1",
        reference: reference.to_owned(),
        created: create,
        tip: s.tip.map(|t| t.to_string()),
        written: s.written,
        removed: s.removed,
        already,
    })
}

#[derive(Debug, Serialize)]
pub struct MergeOut {
    pub schema: &'static str,
    /// The pending merge commit; None when already up to date.
    pub changeset: Option<String>,
    pub written: usize,
    pub removed: usize,
}

impl Render for MergeOut {
    fn text(&self) -> String {
        match &self.changeset {
            Some(c) => format!(
                "merged as {} ({} written, {} removed; run `scn land` to publish)",
                short(c),
                self.written,
                self.removed
            ),
            None => "already up to date".to_owned(),
        }
    }
}

pub async fn merge(ctx: &Ctx, other: &str, message: Option<&str>, author: Option<&str>) -> Result<MergeOut, CliError> {
    let author = resolve_author(author)?;
    let (mut wc, client) = open_wc(ctx).await?;
    let o = wc
        .merge(
            &client,
            other,
            message,
            author.as_ref().map(|(n, e)| (n.as_str(), e.as_str())),
        )
        .await?;
    Ok(match o {
        scion_wc::MergeOutcome::UpToDate => MergeOut {
            schema: "scion.cli.merge/v1",
            changeset: None,
            written: 0,
            removed: 0,
        },
        scion_wc::MergeOutcome::Merged { head, written, removed } => MergeOut {
            schema: "scion.cli.merge/v1",
            changeset: Some(head.to_string()),
            written,
            removed,
        },
    })
}

/// `--author`, else the configured author (scn config, then git); a hint when there is none.
fn resolve_author(author: Option<&str>) -> Result<Option<(String, String)>, CliError> {
    Ok(match author {
        Some(a) => Some(parse_author(a)?),
        None => {
            let settings = crate::config::load(&crate::config::path(|k| std::env::var(k).ok()))?;
            let found = crate::config::author(&settings, crate::config::git_config);
            if found.is_none() {
                eprintln!(
                    "hint: no author configured; set one with `scn config set user.name \"Your Name\"` \
                     and `scn config set user.email you@example.com` (or pass --author)"
                );
            }
            found
        }
    })
}

/// `message` may be omitted only when finishing a sync or merge that stopped on conflicts (it
/// then defaults to that commit's message).
pub async fn commit(ctx: &Ctx, message: Option<&str>, author: Option<&str>) -> Result<CommitOut, CliError> {
    if let Some(a) = author {
        parse_author(a)?;
    }
    let (mut wc, client) = open_wc(ctx).await?;
    let message = match (message, &wc.state().conflict) {
        (Some(m), _) => m.to_owned(),
        (None, Some(_)) => String::new(),
        (None, None) => return Err(CliError::Usage("a commit message is required (-m)".into())),
    };
    let message = message.as_str();
    // The finished sync or merge keeps its own author unless one is given
    let author = match (author, &wc.state().conflict) {
        (None, Some(c)) if c.author.is_some() => None,
        (a, _) => resolve_author(a)?,
    };
    let root = wc.state().base.is_none() && wc.state().head.is_none();
    let created = wc
        .commit_with_stat(&client, message, author.as_ref().map(|(n, e)| (n.as_str(), e.as_str())))
        .await?;
    Ok(commit_out(&wc, root, created))
}

#[derive(Debug, Serialize)]
pub struct LandOut {
    pub schema: &'static str,
    #[serde(rename = "ref")]
    pub reference: String,
    pub tip: String,
    pub version: u64,
    /// 服务端是否 rebase 了提交（ref 在提交之后前进过）。
    pub rebased: bool,
    /// For the text: the repository's URL, the branch's commit before (None: a new branch), and
    /// whether the commit was already on the branch.
    #[serde(skip)]
    pub url: String,
    #[serde(skip)]
    pub old: Option<String>,
    #[serde(skip)]
    pub up_to_date: bool,
}

/// Like `git push`: `To URL`, then `   old..new  main -> main`.
impl Render for LandOut {
    fn text(&self) -> String {
        if self.up_to_date {
            return "Everything up-to-date".to_owned();
        }
        let r = &self.reference;
        let update = match &self.old {
            Some(old) => format!("   {}..{}  {r} -> {r}", short(old), short(&self.tip)),
            None => format!(" * [new branch]      {r} -> {r}"),
        };
        let rebased = if self.rebased {
            format!(" (rebased onto the latest {r})")
        } else {
            String::new()
        };
        format!("To {}\n{update}{rebased}", self.url)
    }
}

pub async fn land(ctx: &Ctx) -> Result<LandOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let old = wc.state().base.clone();
    let landed = wc.land(&client).await?;
    Ok(LandOut {
        schema: "scion.cli.land/v1",
        reference: wc.state().reference.clone(),
        tip: landed.tip.to_string(),
        version: landed.version,
        rebased: landed.rebased(),
        url: format!(
            "{}/{}",
            ctx.endpoint_or(Some(&wc)).trim_end_matches('/'),
            wc.state().repo
        ),
        old,
        up_to_date: landed.landed.is_empty(),
    })
}

#[derive(Debug, Serialize)]
pub struct SyncOut {
    pub schema: &'static str,
    /// up_to_date | updated | rebased
    pub outcome: &'static str,
    pub base: Option<String>,
    pub pending: Option<String>,
    pub written: usize,
    pub removed: usize,
    /// For the text: the branch, and the commit the working copy was on before.
    #[serde(skip)]
    pub reference: String,
    #[serde(skip)]
    pub old: Option<String>,
}

/// Like `git pull`: `Already up to date.`, `Updating a..b` / `Fast-forward`, or for a pending
/// commit `git pull --rebase`'s `Successfully rebased and updated main.`
impl Render for SyncOut {
    fn text(&self) -> String {
        let n = self.written + self.removed;
        let stat = format!(" {n} file{} changed", if n == 1 { "" } else { "s" });
        let updating = match (&self.old, &self.base) {
            (Some(old), Some(new)) => format!("Updating {}..{}\n", short(old), short(new)),
            _ => String::new(),
        };
        match self.outcome {
            "up_to_date" => "Already up to date.".to_owned(),
            "rebased" => format!("{updating}{stat}\nSuccessfully rebased and updated {}.", self.reference),
            _ => format!("{updating}Fast-forward\n{stat}"),
        }
    }
}

pub fn sync_out(wc: &WorkingCopy, old: Option<String>, o: &SyncOutcome) -> SyncOut {
    let (outcome, written, removed) = match o {
        SyncOutcome::UpToDate => ("up_to_date", 0, 0),
        SyncOutcome::Updated { written, removed, .. } => ("updated", *written, *removed),
        SyncOutcome::Rebased { written, removed, .. } => ("rebased", *written, *removed),
    };
    SyncOut {
        schema: "scion.cli.sync/v1",
        outcome,
        base: wc.state().base.clone(),
        pending: wc.state().head.clone(),
        written,
        removed,
        reference: wc.state().reference.clone(),
        old,
    }
}

pub async fn sync(ctx: &Ctx) -> Result<SyncOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let old = wc.state().base.clone();
    let o = wc.sync(&client).await?;
    Ok(sync_out(&wc, old, &o))
}

// ---------- import ----------

#[derive(Debug, Serialize)]
pub struct ImportOut {
    pub schema: &'static str,
    pub repo: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commits: usize,
    /// Commits created by this import (the others were already on the server).
    pub new_commits: usize,
    pub skipped_gitlinks: usize,
    pub tip: Option<String>,
}

impl Render for ImportOut {
    fn text(&self) -> String {
        let mut s = match &self.tip {
            Some(t) => format!(
                "imported {} commits into {} ({} at {}; {} new, {} already there)",
                self.commits,
                self.repo,
                self.reference,
                short(t),
                self.new_commits,
                self.commits - self.new_commits
            ),
            None => format!("nothing to import into {}", self.repo),
        };
        if self.skipped_gitlinks > 0 {
            s.push_str(&format!(
                "\nskipped {} submodule entries (not supported yet)",
                self.skipped_gitlinks
            ));
        }
        s
    }
}

/// 把本地 git 仓库的历史导入 Scion。
pub async fn import_git(ctx: &Ctx, git_dir: &Path, git_ref: &str, target_ref: &str) -> Result<ImportOut, CliError> {
    let repo = ctx
        .repo
        .clone()
        .ok_or_else(|| CliError::Usage("--repo is required for import".into()))?;
    let endpoint = ctx.endpoint.clone().unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
    let client = connect(&endpoint, Some(&repo)).await?;
    let meter = Meter::stderr(ctx.chatty);
    let summary = scion_gitimport::import(&client, &ctx.cwd.join(git_dir), git_ref, target_ref, |done, total| {
        meter.update("Importing commits", done as u64, Some(total as u64), None);
    })
    .await?;
    meter.done();
    Ok(ImportOut {
        schema: "scion.cli.import/v1",
        repo,
        reference: target_ref.to_owned(),
        commits: summary.commits,
        new_commits: summary.new_commits,
        skipped_gitlinks: summary.skipped_gitlinks,
        tip: summary.tip.map(|t| t.to_string()),
    })
}

// ---------- cat / ls（不需要工作副本） ----------

/// 拆分 `rev:path`；没有 `:` 时整个视为路径，rev 用默认值。
pub(crate) fn split_spec<'a>(spec: &'a str, default_rev: &'a str) -> (&'a str, &'a str) {
    match spec.split_once(':') {
        Some((rev, path)) if !rev.is_empty() => (rev, path),
        Some((_, path)) => (default_rev, path),
        None => (default_rev, spec),
    }
}

/// `[REV:][DIR]` for commands that take a tree (`ls`, `archive`): like [`split_spec`], but a bare
/// argument that names a revision is the revision, as in `git archive main` / `git ls-tree main`.
/// `:DIR` forces a path.
pub(crate) async fn split_tree_spec(
    client: &Client,
    ctx: &Ctx,
    spec: &str,
    default_rev: &str,
) -> Result<(String, String), CliError> {
    if spec.is_empty() || spec.contains(':') {
        let (rev, path) = split_spec(spec, default_rev);
        return Ok((ctx.expand_rev(rev)?, path.to_owned()));
    }
    let rev = ctx.expand_rev(spec)?;
    match client.resolve_revision(&rev).await {
        Ok(_) => Ok((rev, String::new())),
        Err(e) if e.is_not_found() => Ok((ctx.expand_rev(default_rev)?, spec.to_owned())),
        Err(e) => Err(e.into()),
    }
}

/// 不需要工作副本的命令：仓库来自 --repo 或当前工作副本。
pub(crate) async fn query_client(ctx: &Ctx) -> Result<(Client, String), CliError> {
    let wc = ctx.working_copy();
    let repo = ctx
        .repo
        .clone()
        .or_else(|| wc.as_ref().map(|w| w.state().repo.clone()))
        .ok_or_else(|| CliError::Usage("--repo is required outside a working copy".into()))?;
    let default_rev = wc
        .as_ref()
        .map_or_else(|| DEFAULT_REF.to_owned(), |w| w.state().reference.clone());
    Ok((connect(&ctx.endpoint_or(wc.as_ref()), Some(&repo)).await?, default_rev))
}

#[derive(Debug, Serialize)]
pub struct CatOut {
    pub schema: &'static str,
    pub revision: String,
    pub path: String,
    pub size: usize,
    /// 内容不是合法 UTF-8 时为 true，此时 `content` 为有损转换结果。
    pub binary: bool,
    pub content: String,
    #[serde(skip)]
    pub data: Bytes,
}

impl Render for CatOut {
    fn text(&self) -> String {
        self.content.clone()
    }
}

pub async fn cat(ctx: &Ctx, spec: &str) -> Result<CatOut, CliError> {
    let (client, default_rev) = query_client(ctx).await?;
    let (rev, path) = split_spec(spec, &default_rev);
    let rev = ctx.expand_rev(rev)?;
    let rev = rev.as_str();
    let data = client.read_file(rev, path).await?;
    let binary = std::str::from_utf8(&data).is_err();
    Ok(CatOut {
        schema: "scion.cli.cat/v1",
        revision: rev.to_owned(),
        path: path.to_owned(),
        size: data.len(),
        binary,
        content: String::from_utf8_lossy(&data).into_owned(),
        data,
    })
}

#[derive(Debug, Serialize)]
pub struct ArchiveOut {
    pub schema: &'static str,
    pub revision: String,
    pub path: String,
    pub changeset: String,
    pub format: String,
    pub bytes: u64,
    /// 输出文件；为空表示写到了标准输出。
    pub output: Option<String>,
}

impl Render for ArchiveOut {
    fn text(&self) -> String {
        format!(
            "wrote {} ({} bytes, {})",
            self.output.as_deref().unwrap_or("-"),
            self.bytes,
            short(&self.changeset)
        )
    }
}

/// 打包下载 `[REV:][DIR]`；`output` 为空时写到 `out`（标准输出）。
pub async fn archive(
    ctx: &Ctx,
    spec: Option<&str>,
    format: &str,
    prefix: &str,
    output: Option<&Path>,
    out: &mut (dyn std::io::Write + Send),
) -> Result<ArchiveOut, CliError> {
    let fmt = scion_client::ArchiveFormat::parse(format)
        .ok_or_else(|| CliError::Usage(format!("unknown archive format {format:?} (tar.gz, zip or tar)")))?;
    let (client, default_rev) = query_client(ctx).await?;
    let (rev, path) = split_tree_spec(&client, ctx, spec.unwrap_or(""), &default_rev).await?;
    let (rev, path) = (rev.as_str(), path.as_str());
    let (changeset, bytes) = match output {
        Some(file) => {
            let f =
                std::fs::File::create(file).map_err(|e| CliError::Usage(format!("create {}: {e}", file.display())))?;
            let mut w = std::io::BufWriter::new(f);
            let r = client.archive(rev, path, fmt, prefix, &mut w).await;
            let flushed = std::io::Write::flush(&mut w);
            if r.is_err() {
                // 不留下不完整的归档
                let _ = std::fs::remove_file(file);
            }
            let r = r?;
            flushed.map_err(|e| CliError::Usage(format!("write {}: {e}", file.display())))?;
            r
        }
        None => client.archive(rev, path, fmt, prefix, out).await?,
    };
    Ok(ArchiveOut {
        schema: "scion.cli.archive/v1",
        revision: rev.to_owned(),
        path: path.to_owned(),
        changeset: changeset.to_string(),
        format: format.to_owned(),
        bytes,
        output: output.map(|p| p.display().to_string()),
    })
}

#[derive(Debug, Serialize)]
pub struct LsEntry {
    pub name: String,
    /// file | executable | symlink | dir
    pub kind: &'static str,
    pub size: u64,
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct LsOut {
    pub schema: &'static str,
    pub revision: String,
    pub path: String,
    pub entries: Vec<LsEntry>,
}

impl Render for LsOut {
    fn text(&self) -> String {
        self.entries
            .iter()
            .map(|e| match e.kind {
                "dir" => format!("{:>10}  {}/", "-", e.name),
                "executable" => format!("{:>10}  {}*", e.size, e.name),
                "symlink" => format!("{:>10}  {}@", e.size, e.name),
                _ => format!("{:>10}  {}", e.size, e.name),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn kind(m: EntryMode) -> &'static str {
    match m {
        EntryMode::File => "file",
        EntryMode::Executable => "executable",
        EntryMode::Symlink => "symlink",
        EntryMode::Tree => "dir",
    }
}

pub async fn ls(ctx: &Ctx, spec: Option<&str>) -> Result<LsOut, CliError> {
    let (client, default_rev) = query_client(ctx).await?;
    let (rev, path) = split_tree_spec(&client, ctx, spec.unwrap_or(""), &default_rev).await?;
    let (rev, path) = (rev.as_str(), path.as_str());
    let entries = client.list_dir(rev, path).await?;
    Ok(LsOut {
        schema: "scion.cli.ls/v1",
        revision: rev.to_owned(),
        path: path.to_owned(),
        entries: entries
            .into_iter()
            .map(|e| LsEntry {
                name: e.name,
                kind: kind(e.mode),
                size: e.size,
                id: e.id.to_string(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wc_relative_paths() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("wc");
        std::fs::create_dir_all(root.join("src/a")).expect("mkdir");
        let rel =
            |cwd: &Path, p: &[&str]| wc_relative(&root, cwd, &p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            rel(&root, &["src/x.rs", ".", "src/"]).expect("ok"),
            ["src/x.rs", ".", "src"]
        );
        assert_eq!(
            rel(&root.join("src/a"), &["../b.rs", ".", "..", "../.."]).expect("ok"),
            ["src/b.rs", "src/a", "src", "."]
        );
        assert!(matches!(rel(&root, &["../x"]), Err(CliError::Usage(_))));
        assert!(matches!(rel(d.path(), &["x"]), Err(CliError::Usage(_))), "cwd outside");
    }

    #[test]
    fn parse_author_cases() {
        assert_eq!(
            parse_author("Ada Lovelace <ada@example.com>").expect("ok"),
            ("Ada Lovelace".into(), "ada@example.com".into())
        );
        for bad in ["Ada", "<a@b>", "Ada <>", "Ada <a@b", ""] {
            assert!(matches!(parse_author(bad), Err(CliError::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn expand_rev_outside_a_working_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = Ctx {
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        assert_eq!(ctx.expand_rev("main~1").ok().as_deref(), Some("main~1"));
        assert_eq!(
            ctx.expand_rev("HEADS-up").ok().as_deref(),
            Some("HEADS-up"),
            "a ref name"
        );
        assert_eq!(ctx.expand_rev("@home").ok().as_deref(), Some("@home"));
        for head in ["HEAD", "@", "HEAD~2", "@^"] {
            assert!(matches!(ctx.expand_rev(head), Err(CliError::Usage(_))), "{head}");
        }
        assert!(!ctx.has_working_copy());
    }

    #[test]
    fn split_spec_cases() {
        assert_eq!(split_spec("main:src/a.rs", "x"), ("main", "src/a.rs"));
        assert_eq!(split_spec("src/a.rs", "main"), ("main", "src/a.rs"));
        assert_eq!(split_spec(":src", "main"), ("main", "src"));
        assert_eq!(split_spec("", "main"), ("main", ""));
        assert_eq!(split_spec("c1abc:", "main"), ("c1abc", ""));
    }

    #[test]
    fn default_dir_is_last_segment() {
        assert_eq!(default_dir("opencsg/model"), "model");
        assert_eq!(default_dir("mono"), "mono");
    }

    #[test]
    fn short_truncates() {
        assert_eq!(short("c1abcdefghijklmn"), "c1abcdefghij");
        assert_eq!(short("c1"), "c1");
    }

    #[test]
    fn kind_all() {
        assert_eq!(kind(EntryMode::File), "file");
        assert_eq!(kind(EntryMode::Executable), "executable");
        assert_eq!(kind(EntryMode::Symlink), "symlink");
        assert_eq!(kind(EntryMode::Tree), "dir");
    }

    #[test]
    fn render_texts() {
        let c = CloneOut {
            schema: "s",
            repo: "r".into(),
            path: "/p".into(),
            reference: "main".into(),
            base: None,
            sparse: None,
        };
        assert_eq!(c.text(), "", "like git clone, nothing on stdout");
        let sp = SparseOut {
            schema: "s",
            dirs: None,
            written: 0,
            removed: 0,
        };
        assert_eq!(sp.text(), "full checkout");

        let s = StatusOut {
            schema: "s",
            reference: "main".into(),
            base: None,
            pending: None,
            added: vec![],
            modified: vec![],
            deleted: vec![],
            conflicted: vec![],
            kinds: Default::default(),
            merging: None,
            short: false,
            cwd: String::new(),
        };
        assert_eq!(
            s.text(),
            "On branch main\n\nNo commits yet\n\nnothing to commit, working tree clean"
        );
        assert_eq!(
            StatusOut {
                short: true,
                ..s.clone()
            }
            .text(),
            ""
        );
        let c = StatusOut {
            base: Some("c1base".into()),
            conflicted: vec!["x".into(), "y".into(), "z".into(), "w".into()],
            modified: vec!["x".into(), "y".into()],
            added: vec!["w".into()],
            kinds: [("y", "modify_delete"), ("z", "delete_modify"), ("w", "add_add")]
                .into_iter()
                .map(|(p, k)| (p.to_owned(), k.to_owned()))
                .collect(),
            merging: Some(true),
            ..s.clone()
        };
        assert_eq!(
            c.text(),
            "On branch main\nYou have unmerged paths.\n  (fix conflicts and run \"scn commit\")\n  \
             (use \"scn merge --abort\" to abort the merge)\n\nUnmerged paths:\n  \
             (use \"scn resolve <file>...\" to mark resolution)\n\tboth modified:   x\n\tdeleted by them: y\n\t\
             deleted by us:   z\n\tboth added:      w"
        );
        assert_eq!(
            StatusOut {
                short: true,
                ..c.clone()
            }
            .text(),
            "UU x\nUD y\nDU z\nAA w"
        );
        for (merging, want) in [
            (Some(true), "All conflicts fixed but you are still merging."),
            (Some(false), "(all conflicts fixed: run \"scn commit\")"),
        ] {
            let done = StatusOut {
                conflicted: vec![],
                merging,
                ..c.clone()
            };
            assert!(done.text().contains(want), "{}", done.text());
        }
        let rebasing = StatusOut {
            merging: Some(false),
            ..c.clone()
        };
        assert!(rebasing.text().contains("(fix conflicts and then run \"scn commit\")"));
        let s = StatusOut {
            added: vec!["a".into()],
            modified: vec!["m".into()],
            deleted: vec!["d".into()],
            base: Some("c1base".into()),
            pending: Some("c1pending0000".into()),
            ..s
        };
        assert_eq!(
            s.text(),
            "On branch main\nYour commit c1pending000 is not landed yet.\n  (use \"scn land\" to publish your commit)\n\n\
             Changes to be committed:\n  (use \"scn restore <file>...\" to discard changes)\n\tnew file:   a\n\t\
             modified:   m\n\tdeleted:    d"
        );
        assert_eq!(
            StatusOut {
                short: true,
                ..s.clone()
            }
            .text(),
            "A  a\nM  m\nD  d"
        );
        let nested = StatusOut {
            short: true,
            cwd: "src/x".into(),
            added: vec!["src/x/a".into(), "src/b".into(), "c".into(), "src".into()],
            modified: vec![],
            deleted: vec![],
            ..s
        };
        assert_eq!(nested.text(), "A  a\nA  ../b\nA  ../../c\nA  ../../src");

        let none = CommitOut {
            schema: "s",
            changeset: None,
            generation: None,
            summary: None,
        };
        assert_eq!(none.text(), "nothing to commit, working tree clean");
        use scion_wc::{FileStat, Mode};
        let stat = |path: &str, old_mode, new_mode| FileStat {
            path: path.into(),
            old_mode,
            new_mode,
        };
        let c = CommitOut {
            changeset: Some("c1abcdefghijklmn".into()),
            generation: Some(1),
            summary: Some(CommitSummary {
                reference: "main".into(),
                subject: "add things".into(),
                root: true,
                files: vec![
                    stat("a", None, Some(Mode::File)),
                    stat("b", Some(Mode::Symlink), None),
                    stat("c", Some(Mode::File), Some(Mode::Executable)),
                    stat("d", Some(Mode::File), Some(Mode::File)),
                ],
            }),
            ..none
        };
        assert_eq!(
            c.text(),
            "[main (root-commit) c1abcdefghij] add things\n 4 files changed\n create mode 100644 a\n \
             delete mode 120000 b\n mode change 100644 => 100755 c"
        );
        let one = CommitOut {
            summary: Some(CommitSummary {
                reference: "dev".into(),
                subject: "s".into(),
                root: false,
                files: vec![stat("d", Some(Mode::File), Some(Mode::File))],
            }),
            ..c
        };
        assert_eq!(one.text(), "[dev c1abcdefghij] s\n 1 file changed");
        let land = LandOut {
            schema: "s",
            reference: "main".into(),
            tip: "c1tipabcdefghij".into(),
            version: 3,
            rebased: false,
            url: "http://h:7080/mono".into(),
            old: None,
            up_to_date: false,
        };
        assert_eq!(land.text(), "To http://h:7080/mono\n * [new branch]      main -> main");
        let land = LandOut {
            old: Some("c1oldabcdefghij".into()),
            rebased: true,
            ..land
        };
        assert_eq!(
            land.text(),
            "To http://h:7080/mono\n   c1oldabcdefg..c1tipabcdefg  main -> main (rebased onto the latest main)"
        );
        let land = LandOut {
            up_to_date: true,
            ..land
        };
        assert_eq!(land.text(), "Everything up-to-date");

        let sw = SwitchOut {
            schema: "s",
            reference: "dev".into(),
            created: true,
            tip: None,
            written: 0,
            removed: 0,
            already: false,
        };
        assert_eq!(sw.text(), "Switched to a new branch 'dev'");
        let sw = SwitchOut { created: false, ..sw };
        assert_eq!(sw.text(), "Switched to branch 'dev'");
        assert_eq!(SwitchOut { already: true, ..sw }.text(), "Already on 'dev'");
        let mut so = SyncOut {
            schema: "s",
            outcome: "up_to_date",
            base: None,
            pending: None,
            written: 0,
            removed: 0,
            reference: "main".into(),
            old: None,
        };
        assert_eq!(so.text(), "Already up to date.");
        so.outcome = "updated";
        so.written = 1;
        assert_eq!(so.text(), "Fast-forward\n 1 file changed");
        so.outcome = "rebased";
        so.base = Some("c1base".into());
        so.old = Some("c1old".into());
        so.removed = 2;
        assert_eq!(
            so.text(),
            "Updating c1old..c1base\n 3 files changed\nSuccessfully rebased and updated main."
        );

        let ls = LsOut {
            schema: "s",
            revision: "main".into(),
            path: String::new(),
            entries: vec![
                LsEntry {
                    name: "d".into(),
                    kind: "dir",
                    size: 0,
                    id: "t1".into(),
                },
                LsEntry {
                    name: "x".into(),
                    kind: "executable",
                    size: 3,
                    id: "b1".into(),
                },
                LsEntry {
                    name: "l".into(),
                    kind: "symlink",
                    size: 1,
                    id: "b1".into(),
                },
                LsEntry {
                    name: "f".into(),
                    kind: "file",
                    size: 2,
                    id: "b1".into(),
                },
            ],
        };
        let t = ls.text();
        assert!(t.contains("d/") && t.contains("x*") && t.contains("l@") && t.ends_with("f"));

        let cat = CatOut {
            schema: "s",
            revision: "main".into(),
            path: "a".into(),
            size: 2,
            binary: false,
            content: "hi".into(),
            data: Bytes::from_static(b"hi"),
        };
        assert_eq!(cat.text(), "hi");
    }

    #[test]
    fn import_render() {
        let o = ImportOut {
            schema: "s",
            repo: "r".into(),
            reference: "main".into(),
            commits: 3,
            new_commits: 2,
            skipped_gitlinks: 1,
            tip: Some("c1tiptiptiptip".into()),
        };
        assert_eq!(
            o.text(),
            "imported 3 commits into r (main at c1tiptiptipt; 2 new, 1 already there)\nskipped 1 submodule entries (not supported yet)"
        );
        let o = ImportOut {
            tip: None,
            skipped_gitlinks: 0,
            ..o
        };
        assert_eq!(o.text(), "nothing to import into r");
    }

    #[test]
    fn split_repo_url_parses_server_and_repository() {
        let ok = |s: &str| split_repo_url(s).expect("valid");
        assert_eq!(ok("mono"), None);
        assert_eq!(ok("org/mono"), None);
        assert_eq!(
            ok("http://host:7080/org/mono"),
            Some(("http://host:7080".into(), "org/mono".into()))
        );
        assert_eq!(ok("https://h/mono/"), Some(("https://h".into(), "mono".into())));
        assert_eq!(ok("http://h"), Some(("http://h".into(), String::new())));
        assert_eq!(ok("http://h/"), Some(("http://h".into(), String::new())));
        for bad in [
            "http:///mono",
            "http://u@h/mono",
            "http://h/a?x=1",
            "http://h/a#f",
            "http://h/a//b",
        ] {
            assert!(matches!(split_repo_url(bad), Err(CliError::Usage(_))), "{bad}");
        }
    }

    #[test]
    fn repository_urls_replace_the_endpoint() {
        let mut ctx = Ctx {
            endpoint: Some("http://flag".into()),
            ..Default::default()
        };
        assert_eq!(ctx.named_repo_arg("mono").expect("name"), "mono");
        assert_eq!(ctx.endpoint.as_deref(), Some("http://flag"));
        assert_eq!(ctx.named_repo_arg("http://h:1/org/mono").expect("url"), "org/mono");
        assert_eq!(ctx.endpoint.as_deref(), Some("http://h:1"));
        assert!(matches!(ctx.named_repo_arg("http://h:2"), Err(CliError::Usage(_))));
        assert_eq!(ctx.repo_arg("http://h:3/").expect("prefix"), "");
        assert_eq!(ctx.endpoint.as_deref(), Some("http://h:3"));

        ctx.server_arg("https://s:4/ignored").expect("server");
        assert_eq!(ctx.endpoint.as_deref(), Some("https://s:4"));
        assert!(matches!(ctx.server_arg("s:4"), Err(CliError::Usage(_))));

        let mut ctx = Ctx::default();
        assert_eq!(
            ctx.fork_args("http://h/a", "b").expect("fork"),
            ("a".to_owned(), "b".to_owned())
        );
        assert_eq!(ctx.endpoint.as_deref(), Some("http://h"));
        assert_eq!(
            ctx.fork_args("http://h/a", "http://h/b").expect("same server"),
            ("a".to_owned(), "b".to_owned())
        );
        assert!(matches!(
            ctx.fork_args("http://h/a", "http://other/b"),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn endpoint_precedence() {
        let ctx = Ctx {
            endpoint: Some("http://flag".into()),
            ..Default::default()
        };
        assert_eq!(ctx.endpoint_or(None), "http://flag");
        assert_eq!(Ctx::default().endpoint_or(None), DEFAULT_ENDPOINT);
    }

    #[tokio::test]
    async fn query_outside_working_copy_needs_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = Ctx {
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        assert!(matches!(cat(&ctx, "main:a").await, Err(CliError::Usage(_))));
        assert!(matches!(ls(&ctx, None).await, Err(CliError::Usage(_))));
        assert!(matches!(
            status(&ctx, false),
            Err(CliError::Wc(scion_wc::Error::NotAWorkingCopy(_)))
        ));
        assert!(matches!(
            commit(&ctx, Some("m"), Some("bad")).await,
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            import_git(&ctx, Path::new("."), "main", "main").await,
            Err(CliError::Usage(_))
        ));
    }
}
