//! 各子命令的实现。每个命令返回可渲染为文本或 JSON 的结果。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use scion_client::{Client, EntryMode};
use scion_wc::{Sparse, Status, SyncOutcome, WorkingCopy};
use serde::Serialize;

use crate::output::{CliError, DoctorOut, Render, doctor_out};

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:7080";
const DEFAULT_REF: &str = "main";

/// 命令行给出的全局选项。
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub endpoint: Option<String>,
    pub repo: Option<String>,
    pub cwd: PathBuf,
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

/// 连接服务端：默认经本机 daemon（必要时拉起它），daemon 不可用时直连。
pub async fn connect(endpoint: &str, repo: Option<&str>) -> Result<Client, CliError> {
    let mut b = Client::builder()
        .endpoint(endpoint)
        .auth(crate::login::auth_for(endpoint, |k| std::env::var(k).ok()))
        .client_name("scn")
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

impl Render for CloneOut {
    fn text(&self) -> String {
        let s = match &self.base {
            Some(b) => format!(
                "cloned {} ({} at {}) into {}",
                self.repo,
                self.reference,
                short(b),
                self.path
            ),
            None => format!(
                "cloned empty repository {} into {} ({} does not exist yet)",
                self.repo, self.path, self.reference
            ),
        };
        match &self.sparse {
            Some(dirs) => format!("{s}\nsparse checkout: {}", sparse_list(dirs)),
            None => s,
        }
    }
}

fn sparse_list(dirs: &[String]) -> String {
    if dirs.is_empty() {
        "root files only".to_owned()
    } else {
        dirs.join(", ")
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
    let target = ctx.cwd.join(dir.unwrap_or_else(|| Path::new(default_dir(repo))));
    let wc = WorkingCopy::clone(&client, &endpoint, repo, reference, &target, sparse).await?;
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

#[derive(Debug, Serialize)]
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
}

impl Render for StatusOut {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        lines.extend(self.conflicted.iter().map(|p| format!("C {p}")));
        lines.extend(self.added.iter().map(|p| format!("A {p}")));
        lines.extend(self.modified.iter().map(|p| format!("M {p}")));
        lines.extend(self.deleted.iter().map(|p| format!("D {p}")));
        if lines.is_empty() {
            lines.push("nothing to commit".to_owned());
        }
        if let Some(p) = &self.pending {
            lines.push(format!("committed, not landed: {} (run `scn land`)", short(p)));
        }
        if !self.conflicted.is_empty() {
            lines.push(
                "fix the conflicted files, then `scn resolve PATH` (or --ours / --theirs), then `scn commit`"
                    .to_owned(),
            );
        }
        lines.join("\n")
    }
}

pub fn status_out(wc: &WorkingCopy, s: Status) -> StatusOut {
    StatusOut {
        schema: "scion.cli.status/v1",
        reference: wc.state().reference.clone(),
        base: wc.state().base.clone(),
        pending: wc.state().head.clone(),
        added: s.added,
        modified: s.modified,
        deleted: s.deleted,
        conflicted: s.conflicted,
    }
}

pub fn status(ctx: &Ctx) -> Result<StatusOut, CliError> {
    let wc = WorkingCopy::open(&ctx.cwd)?;
    let s = wc.status()?;
    Ok(status_out(&wc, s))
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
}

impl Render for CommitOut {
    fn text(&self) -> String {
        match &self.changeset {
            Some(c) => format!("committed {} (run `scn land` to publish)", short(c)),
            None => "nothing to commit".to_owned(),
        }
    }
}

/// `scn commit --amend`: replaces the pending commit. Unlike a new commit, the default author is
/// the old commit's author, not the configured one.
pub async fn amend(ctx: &Ctx, message: Option<&str>, author: Option<&str>) -> Result<CommitOut, CliError> {
    let author = author.map(parse_author).transpose()?;
    let (mut wc, client) = open_wc(ctx).await?;
    let created = wc
        .amend(&client, message, author.as_ref().map(|(n, e)| (n.as_str(), e.as_str())))
        .await?;
    Ok(CommitOut {
        schema: "scion.cli.commit/v1",
        changeset: created.map(|c| c.id.to_string()),
        generation: created.map(|c| c.generation),
    })
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
}

impl Render for SwitchOut {
    fn text(&self) -> String {
        let verb = if self.created {
            "created and switched to"
        } else {
            "switched to"
        };
        match &self.tip {
            Some(t) => format!(
                "{verb} {} at {} ({} written, {} removed)",
                self.reference,
                short(t),
                self.written,
                self.removed
            ),
            None => format!("{verb} {}", self.reference),
        }
    }
}

pub async fn switch(ctx: &Ctx, reference: &str, create: bool) -> Result<SwitchOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let s = wc.switch(&client, reference, create).await?;
    Ok(SwitchOut {
        schema: "scion.cli.switch/v1",
        reference: reference.to_owned(),
        created: create,
        tip: s.tip.map(|t| t.to_string()),
        written: s.written,
        removed: s.removed,
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
    let created = wc
        .commit(&client, message, author.as_ref().map(|(n, e)| (n.as_str(), e.as_str())))
        .await?;
    Ok(CommitOut {
        schema: "scion.cli.commit/v1",
        changeset: created.map(|c| c.id.to_string()),
        generation: created.map(|c| c.generation),
    })
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
}

impl Render for LandOut {
    fn text(&self) -> String {
        let rebased = if self.rebased {
            format!(", rebased onto the latest {}", self.reference)
        } else {
            String::new()
        };
        format!(
            "landed {} on {} (version {}{rebased})",
            short(&self.tip),
            self.reference,
            self.version
        )
    }
}

pub async fn land(ctx: &Ctx) -> Result<LandOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let landed = wc.land(&client).await?;
    Ok(LandOut {
        schema: "scion.cli.land/v1",
        reference: wc.state().reference.clone(),
        tip: landed.tip.to_string(),
        version: landed.version,
        rebased: landed.rebased(),
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
}

impl Render for SyncOut {
    fn text(&self) -> String {
        let base = self.base.as_deref().map(short).unwrap_or("-");
        match self.outcome {
            "up_to_date" => "already up to date".to_owned(),
            "rebased" => format!(
                "rebased your change onto {base} ({} written, {} removed); run `scn land`",
                self.written, self.removed
            ),
            _ => format!("updated to {base} ({} written, {} removed)", self.written, self.removed),
        }
    }
}

pub fn sync_out(wc: &WorkingCopy, o: &SyncOutcome) -> SyncOut {
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
    }
}

pub async fn sync(ctx: &Ctx) -> Result<SyncOut, CliError> {
    let (mut wc, client) = open_wc(ctx).await?;
    let o = wc.sync(&client).await?;
    Ok(sync_out(&wc, &o))
}

// ---------- import ----------

#[derive(Debug, Serialize)]
pub struct ImportOut {
    pub schema: &'static str,
    pub repo: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commits: usize,
    pub skipped_gitlinks: usize,
    pub tip: Option<String>,
}

impl Render for ImportOut {
    fn text(&self) -> String {
        let mut s = match &self.tip {
            Some(t) => format!(
                "imported {} commits into {} ({} at {})",
                self.commits,
                self.repo,
                self.reference,
                short(t)
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
    let summary = scion_gitimport::import(&client, &ctx.cwd.join(git_dir), git_ref, target_ref, |_, _| {}).await?;
    Ok(ImportOut {
        schema: "scion.cli.import/v1",
        repo,
        reference: target_ref.to_owned(),
        commits: summary.commits,
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
    let (rev, path) = split_spec(spec.unwrap_or(""), &default_rev);
    let rev = ctx.expand_rev(rev)?;
    let rev = rev.as_str();
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
    let (rev, path) = split_spec(spec.unwrap_or(""), &default_rev);
    let rev = ctx.expand_rev(rev)?;
    let rev = rev.as_str();
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
        assert!(c.text().contains("empty repository"));
        let c = CloneOut {
            base: Some("c1abcdefghijklmnop".into()),
            ..c
        };
        assert!(c.text().contains("main at c1abcdefghij"));
        let c = CloneOut {
            sparse: Some(vec![]),
            ..c
        };
        assert!(c.text().ends_with("sparse checkout: root files only"));
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
        };
        assert_eq!(s.text(), "nothing to commit");
        let c = StatusOut {
            conflicted: vec!["x".into()],
            modified: vec!["x".into()],
            ..StatusOut {
                schema: "s",
                reference: "main".into(),
                base: None,
                pending: None,
                added: vec![],
                modified: vec![],
                deleted: vec![],
                conflicted: vec![],
            }
        };
        assert!(
            c.text().starts_with("C x\nM x\nfix the conflicted files"),
            "{}",
            c.text()
        );
        let s = StatusOut {
            added: vec!["a".into()],
            modified: vec!["m".into()],
            deleted: vec!["d".into()],
            pending: Some("c1pending0000".into()),
            ..s
        };
        assert_eq!(
            s.text(),
            "A a\nM m\nD d\ncommitted, not landed: c1pending000 (run `scn land`)"
        );

        assert_eq!(
            CommitOut {
                schema: "s",
                changeset: None,
                generation: None
            }
            .text(),
            "nothing to commit"
        );
        assert!(
            CommitOut {
                schema: "s",
                changeset: Some("c1x".into()),
                generation: Some(1)
            }
            .text()
            .contains("scn land")
        );
        assert!(
            LandOut {
                schema: "s",
                reference: "main".into(),
                tip: "c1t".into(),
                version: 3,
                rebased: false,
            }
            .text()
            .contains("version 3)")
        );
        assert!(
            LandOut {
                schema: "s",
                reference: "main".into(),
                tip: "c1t".into(),
                version: 4,
                rebased: true,
            }
            .text()
            .contains("rebased onto the latest main")
        );

        let mut so = SyncOut {
            schema: "s",
            outcome: "up_to_date",
            base: None,
            pending: None,
            written: 0,
            removed: 0,
        };
        assert_eq!(so.text(), "already up to date");
        so.outcome = "updated";
        assert!(so.text().starts_with("updated to -"));
        so.outcome = "rebased";
        so.base = Some("c1base".into());
        assert!(so.text().contains("rebased your change onto c1base"));

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
            skipped_gitlinks: 1,
            tip: Some("c1tiptiptiptip".into()),
        };
        assert_eq!(
            o.text(),
            "imported 3 commits into r (main at c1tiptiptipt)\nskipped 1 submodule entries (not supported yet)"
        );
        let o = ImportOut {
            tip: None,
            skipped_gitlinks: 0,
            ..o
        };
        assert_eq!(o.text(), "nothing to import into r");
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
            status(&ctx),
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
