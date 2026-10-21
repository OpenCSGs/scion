//! `scn log` 与 `scn diff`。

use bytes::Bytes;
use scion_client::{Client, FileChangeInfo, LogItem, ObjectId};
use scion_wc::{Mode, WorkingCopy};
use serde::Serialize;

use crate::commands::{Ctx, query_client, split_spec};
use crate::output::{CliError, Render};

/// Unix 毫秒 + 时区分钟 → `YYYY-MM-DD HH:MM +HHMM`（提交者所在时区的本地时间）。
pub fn format_time(time_ms: i64, tz_offset_minutes: i32) -> String {
    let secs = time_ms.div_euclid(1000) + i64::from(tz_offset_minutes) * 60;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
    let tz = tz_offset_minutes.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        tz / 60,
        tz % 60
    )
}

/// git's default date format: `Wed Nov 15 06:13:20 2023 +0800`, in the author's time zone.
pub fn git_date(time_ms: i64, tz_offset_minutes: i32) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = time_ms.div_euclid(1000) + i64::from(tz_offset_minutes) * 60;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
    let tz = tz_offset_minutes.unsigned_abs();
    format!(
        "{} {} {d} {:02}:{:02}:{:02} {y} {sign}{:02}{:02}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[m as usize - 1],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        tz / 60,
        tz % 60
    )
}

/// 1970-01-01 起的天数 → 公历 (年, 月, 日)。算法来自 Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn short(id: &ObjectId) -> String {
    id.to_string()[..12].to_owned()
}

// ---------- log ----------

#[derive(Debug, Serialize)]
pub struct LogLine {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author date (what `git log` shows).
    pub time_ms: i64,
    pub tz_offset_minutes: i32,
    pub committer: String,
    pub committer_time_ms: i64,
    pub message: String,
    pub generation: u64,
}

#[derive(Debug, Serialize)]
pub struct LogOut {
    pub schema: &'static str,
    pub revision: String,
    pub path: Option<String>,
    pub entries: Vec<LogLine>,
    /// `--oneline`.
    #[serde(skip)]
    pub oneline: bool,
}

/// A commit as `git log` and `git show` print it: `commit`, `Merge:`, `Author:`, `Date:`, then
/// the message indented by four spaces.
fn commit_header(c: &LogLine) -> String {
    let mut out = format!("commit {}\n", c.id);
    if c.parents.len() > 1 {
        let short: Vec<&str> = c.parents.iter().map(|p| &p[..p.len().min(12)]).collect();
        out.push_str(&format!("Merge: {}\n", short.join(" ")));
    }
    out.push_str(&format!(
        "Author: {} <{}>\nDate:   {}",
        c.author,
        c.email,
        git_date(c.time_ms, c.tz_offset_minutes)
    ));
    if !c.message.trim().is_empty() {
        out.push('\n');
        for line in c.message.lines() {
            out.push_str(&format!("\n    {line}"));
        }
    }
    out
}

/// Like `git log`, or `git log --oneline`.
impl Render for LogOut {
    fn text(&self) -> String {
        if self.oneline {
            return self
                .entries
                .iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        &e.id[..12.min(e.id.len())],
                        e.message.lines().next().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        self.entries.iter().map(commit_header).collect::<Vec<_>>().join("\n\n")
    }
}

fn log_line(e: LogItem) -> LogLine {
    LogLine {
        id: e.id.to_string(),
        parents: e.parents.iter().map(ToString::to_string).collect(),
        // Author and author date, as `git log` shows them
        time_ms: e.author.time_ms,
        tz_offset_minutes: e.author.tz_offset_minutes,
        committer: e.committer.name,
        committer_time_ms: e.committer.time_ms,
        author: e.author.name,
        email: e.author.email,
        message: e.message,
        generation: e.generation,
    }
}

/// 自动翻页直到凑够 `limit` 条或历史结束。
pub async fn log(
    ctx: &Ctx,
    revision: Option<&str>,
    path: Option<&str>,
    limit: usize,
    oneline: bool,
) -> Result<LogOut, CliError> {
    let (client, default_rev) = query_client(ctx).await?;
    let revision = ctx.expand_rev(&revision.map_or(default_rev, str::to_owned))?;
    let mut entries = Vec::new();
    let mut token: Option<String> = None;
    while entries.len() < limit {
        let want = u32::try_from(limit - entries.len()).unwrap_or(u32::MAX).min(1000);
        let page = client.log(&revision, path, want, token.as_deref()).await?;
        entries.extend(page.entries.into_iter().map(log_line));
        match page.next_page_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    entries.truncate(limit);
    Ok(LogOut {
        schema: "scion.cli.log/v1",
        revision,
        path: path.map(str::to_owned),
        entries,
        oneline,
    })
}

// ---------- blame ----------

#[derive(Debug, Serialize)]
pub struct BlameLineOut {
    pub line: usize,
    pub commit: String,
    pub author: String,
    pub time_ms: i64,
    pub tz_offset_minutes: i32,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct BlameOut {
    pub schema: &'static str,
    pub revision: String,
    pub path: String,
    pub lines: Vec<BlameLineOut>,
}

impl Render for BlameOut {
    fn text(&self) -> String {
        let width = self.lines.last().map_or(1, |l| l.line.to_string().len());
        self.lines
            .iter()
            .map(|l| {
                let date = &format_time(l.time_ms, l.tz_offset_minutes)[..10];
                let author: String = l.author.chars().take(16).collect();
                format!(
                    "{} ({author:<16} {date}) {:>width$}| {}",
                    &l.commit[..12],
                    l.line,
                    l.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 按行切分，与服务端一致：保留换行符，末尾没有换行的最后一行也算一行。
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|b| *b == b'\n').collect()
}

pub async fn blame(ctx: &Ctx, spec: &str) -> Result<BlameOut, CliError> {
    let (client, default_rev) = query_client(ctx).await?;
    let (rev, path) = split_spec(spec, &default_rev);
    let rev = ctx.expand_rev(rev)?;
    let rev = rev.as_str();
    let info = client.blame(rev, path).await?;
    let data = client.get_object(&info.blob).await?;
    let raw = split_lines(&data);
    if raw.len() != info.lines.len() {
        return Err(CliError::Client(scion_client::Error::Protocol(format!(
            "blame covers {} lines but the file has {}",
            info.lines.len(),
            raw.len()
        ))));
    }
    let lines = raw
        .iter()
        .zip(&info.lines)
        .enumerate()
        .map(|(i, (text, idx))| {
            let c = &info.commits[*idx];
            let content = String::from_utf8_lossy(text);
            BlameLineOut {
                line: i + 1,
                commit: c.id.to_string(),
                author: c.author.name.clone(),
                time_ms: c.author.time_ms,
                tz_offset_minutes: c.author.tz_offset_minutes,
                content: content.trim_end_matches(['\n', '\r']).to_owned(),
            }
        })
        .collect();
    Ok(BlameOut {
        schema: "scion.cli.blame/v1",
        revision: rev.to_owned(),
        path: path.to_owned(),
        lines,
    })
}

// ---------- diff ----------

#[derive(Debug, Serialize)]
pub struct DiffFile {
    pub path: String,
    /// A（新增）| M（修改）| D（删除）
    pub status: &'static str,
    pub old_id: Option<String>,
    pub new_id: Option<String>,
    pub old_mode: Option<&'static str>,
    pub new_mode: Option<&'static str>,
    pub binary: bool,
    /// unified diff；`--name-only` 或二进制文件时为 None。
    pub patch: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffOut {
    pub schema: &'static str,
    pub from: Option<String>,
    pub to: String,
    pub files: Vec<DiffFile>,
    #[serde(skip)]
    pub name_only: bool,
}

impl Render for DiffOut {
    fn text(&self) -> String {
        if self.files.is_empty() {
            return "no differences".to_owned();
        }
        if self.name_only {
            return self
                .files
                .iter()
                .map(|f| format!("{} {}", f.status, f.path))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let mut out = String::new();
        for f in &self.files {
            out.push_str(&format!("diff --scion a/{0} b/{0}\n", f.path));
            match (f.old_mode, f.new_mode) {
                (None, Some(m)) => out.push_str(&format!("new file ({m})\n")),
                (Some(m), None) => out.push_str(&format!("deleted file ({m})\n")),
                (Some(a), Some(b)) if a != b => out.push_str(&format!("mode change {a} => {b}\n")),
                _ => {}
            }
            if f.binary {
                out.push_str(&format!("Binary files a/{0} and b/{0} differ\n", f.path));
            } else if let Some(p) = &f.patch {
                out.push_str(p);
            }
        }
        out.trim_end().to_owned()
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::File => "file",
        Mode::Executable => "executable",
        Mode::Symlink => "symlink",
    }
}

fn entry_mode_name(m: scion_client::EntryMode) -> &'static str {
    crate::commands::kind(m)
}

fn is_binary(b: &[u8]) -> bool {
    b.contains(&0) || std::str::from_utf8(b).is_err()
}

/// 生成 unified diff；任一侧为二进制时返回 (true, None)。
/// The line-diff algorithm to use: exact LCS (minimal, like `git diff --minimal`) when the part
/// between the common prefix and suffix is small enough for its table (about 32 MB at most),
/// else Myers. similar's Myers is not always minimal on large files.
fn diff_algorithm(old: &[u8], new: &[u8]) -> similar::Algorithm {
    const MAX_LCS_CELLS: usize = 4_000_000;
    let (a, b): (Vec<&[u8]>, Vec<&[u8]>) = (
        old.split_inclusive(|c| *c == b'\n').collect(),
        new.split_inclusive(|c| *c == b'\n').collect(),
    );
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    if (a.len() - pre - suf).saturating_mul(b.len() - pre - suf) <= MAX_LCS_CELLS {
        similar::Algorithm::Lcs
    } else {
        similar::Algorithm::Myers
    }
}

pub fn render_patch(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> (bool, Option<String>) {
    let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
    if is_binary(old) || is_binary(new) {
        return (true, None);
    }
    let (a, b) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    let diff = similar::TextDiff::configure()
        .algorithm(diff_algorithm(a.as_bytes(), b.as_bytes()))
        .diff_lines(a.as_ref(), b.as_ref());
    let patch = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    (false, Some(patch))
}

fn status_of(old: bool, new: bool) -> &'static str {
    match (old, new) {
        (false, _) => "A",
        (_, false) => "D",
        _ => "M",
    }
}

async fn fetch(client: &Client, id: Option<ObjectId>) -> Result<Option<Bytes>, CliError> {
    match id {
        Some(id) => Ok(Some(client.get_object(&id).await?)),
        None => Ok(None),
    }
}

/// 服务端算出文件列表，客户端取内容生成 patch。
async fn remote_files(
    client: &Client,
    changes: Vec<FileChangeInfo>,
    name_only: bool,
) -> Result<Vec<DiffFile>, CliError> {
    let mut files = Vec::with_capacity(changes.len());
    for c in changes {
        let (old_id, new_id) = (c.old.as_ref().map(|e| e.id), c.new.as_ref().map(|e| e.id));
        let (binary, patch) = if name_only {
            (false, None)
        } else {
            let (o, n) = (fetch(client, old_id).await?, fetch(client, new_id).await?);
            render_patch(&c.path, o.as_deref(), n.as_deref())
        };
        files.push(DiffFile {
            status: status_of(c.old.is_some(), c.new.is_some()),
            old_id: old_id.map(|i| i.to_string()),
            new_id: new_id.map(|i| i.to_string()),
            old_mode: c.old.as_ref().map(|e| entry_mode_name(e.mode)),
            new_mode: c.new.as_ref().map(|e| entry_mode_name(e.mode)),
            binary,
            patch,
            path: c.path,
        });
    }
    Ok(files)
}

/// git's range forms: `A..B` → (A, B, false); `A...B` → (A, B, true), meaning "from the merge
/// base of A and B". An empty side means HEAD.
pub(crate) fn split_range(rev: &str) -> Option<(&str, &str, bool)> {
    if let Some((a, b)) = rev.split_once("...") {
        return Some((a, b, true));
    }
    rev.split_once("..").map(|(a, b)| (a, b, false))
}

/// - 不带版本：工作副本里尚未提交的改动；
/// - 一个版本：该提交相对第一个父提交引入的改动；
/// - 两个版本，或 `A..B`：两者之间的差异；`A...B`：B 相对两者的公共祖先的改动。
pub async fn diff(ctx: &Ctx, revs: &[String], path: Option<&str>, name_only: bool) -> Result<DiffOut, CliError> {
    let or_head = |r: &str| if r.is_empty() { "HEAD".to_owned() } else { r.to_owned() };
    let (from, to) = match revs {
        [] => return local_diff(ctx, path, name_only).await,
        [r] => match split_range(r) {
            Some((a, b, three_dots)) => {
                let (a, b) = (ctx.expand_rev(&or_head(a))?, ctx.expand_rev(&or_head(b))?);
                if three_dots {
                    let (client, _) = query_client(ctx).await?;
                    let base = client
                        .merge_bases(&a, &b)
                        .await?
                        .first()
                        .map(ToString::to_string)
                        .ok_or_else(|| CliError::Usage(format!("{a} and {b} have no common ancestor")))?;
                    (Some(base), b)
                } else {
                    (Some(a), b)
                }
            }
            None => {
                let rev = ctx.expand_rev(r)?;
                let (client, _) = query_client(ctx).await?;
                let page = client.log(&rev, None, 1, None).await?;
                let entry = page
                    .entries
                    .into_iter()
                    .next()
                    .ok_or_else(|| CliError::Usage(format!("{rev} has no history")))?;
                (entry.parents.first().map(ToString::to_string), entry.id.to_string())
            }
        },
        [a, b] => (Some(ctx.expand_rev(a)?), ctx.expand_rev(b)?),
        _ => return Err(CliError::Usage("scn diff takes at most two revisions".into())),
    };
    let (client, _) = query_client(ctx).await?;
    let changes = client.diff(from.as_deref(), &to, path).await?;
    Ok(DiffOut {
        schema: "scion.cli.diff/v1",
        files: remote_files(&client, changes, name_only).await?,
        from,
        to,
        name_only,
    })
}

#[derive(Debug, Serialize)]
pub struct ShowOut {
    pub schema: &'static str,
    pub commit: LogLine,
    pub diff: DiffOut,
}

/// Like `git show`: the commit as `git log` prints it, then its diff.
impl Render for ShowOut {
    fn text(&self) -> String {
        let mut out = commit_header(&self.commit);
        let diff = self.diff.text();
        if !diff.is_empty() {
            out.push_str("\n\n");
            out.push_str(&diff);
        }
        out
    }
}

/// `scn show [REV] [-- PATH]`: the commit's metadata and the changes it introduced (relative to
/// its first parent). REV defaults to HEAD in a working copy, else to the default ref.
pub async fn show(ctx: &Ctx, revision: Option<&str>, path: Option<&str>, name_only: bool) -> Result<ShowOut, CliError> {
    let (client, default_rev) = query_client(ctx).await?;
    let rev = match revision {
        Some(r) => ctx.expand_rev(r)?,
        None if ctx.has_working_copy() => ctx.expand_rev("HEAD")?,
        None => default_rev,
    };
    let page = client.log(&rev, None, 1, None).await?;
    let entry = page
        .entries
        .into_iter()
        .next()
        .ok_or_else(|| CliError::Usage(format!("{rev} has no history")))?;
    let id = entry.id.to_string();
    let from = entry.parents.first().map(ToString::to_string);
    let changes = client.diff(from.as_deref(), &id, path).await?;
    Ok(ShowOut {
        schema: "scion.cli.show/v1",
        diff: DiffOut {
            schema: "scion.cli.diff/v1",
            files: remote_files(&client, changes, name_only).await?,
            from,
            to: id,
            name_only,
        },
        commit: log_line(entry),
    })
}

fn in_path(file: &str, path: Option<&str>) -> bool {
    match path.filter(|p| !p.is_empty()) {
        None => true,
        Some(p) => file == p || file.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')),
    }
}

async fn local_diff(ctx: &Ctx, path: Option<&str>, name_only: bool) -> Result<DiffOut, CliError> {
    let wc = WorkingCopy::open(&ctx.cwd)?;
    let (client, _) = query_client(ctx).await?;
    let mut changes = wc.local_changes()?;
    changes.retain(|c| in_path(&c.path, path));
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut files = Vec::with_capacity(changes.len());
    for c in changes {
        let (binary, patch) = if name_only {
            (false, None)
        } else {
            let old = fetch(&client, c.old.map(|(_, id)| id)).await?;
            render_patch(&c.path, old.as_deref(), c.new.as_ref().map(|(_, b)| b.as_ref()))
        };
        files.push(DiffFile {
            status: status_of(c.old.is_some(), c.new.is_some()),
            old_id: c.old.map(|(_, id)| id.to_string()),
            new_id: c.new.as_ref().map(|(_, b)| ObjectId::of_blob(b).to_string()),
            old_mode: c.old.map(|(m, _)| mode_name(m)),
            new_mode: c.new.as_ref().map(|(m, _)| mode_name(*m)),
            binary,
            patch,
            path: c.path,
        });
    }
    let from = wc.state().current()?.map(|id| short(&id));
    Ok(DiffOut {
        schema: "scion.cli.diff/v1",
        from,
        to: "working copy".to_owned(),
        files,
        name_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(split_range("a..b"), Some(("a", "b", false)));
        assert_eq!(split_range("main...feat"), Some(("main", "feat", true)));
        assert_eq!(split_range("..b"), Some(("", "b", false)));
        assert_eq!(split_range("a..."), Some(("a", "", true)));
        assert_eq!(split_range("main~1"), None);
    }

    #[test]
    fn show_text() {
        let commit = LogLine {
            id: "c1aaaaaaaaaaaaaaaa".into(),
            parents: vec!["c1bbbbbbbbbbbbbbbb".into(), "c1cccccccccccccccc".into()],
            author: "Ada".into(),
            email: "ada@x".into(),
            time_ms: 0,
            tz_offset_minutes: 0,
            committer: "c".into(),
            committer_time_ms: 0,
            message: "subject\n\nbody".into(),
            generation: 3,
        };
        let diff = DiffOut {
            schema: "d",
            from: None,
            to: "x".into(),
            files: vec![],
            name_only: true,
        };
        let text = ShowOut {
            schema: "s",
            commit,
            diff,
        }
        .text();
        assert!(
            text.starts_with(
                "commit c1aaaaaaaaaaaaaaaa\nMerge: c1bbbbbbbbbb c1cccccccccc\nAuthor: Ada <ada@x>\nDate:   Thu Jan 1 00:00:00 1970 +0000\n\n    subject\n    \n    body"
            ),
            "{text}"
        );
    }

    #[test]
    fn git_dates() {
        assert_eq!(git_date(0, 0), "Thu Jan 1 00:00:00 1970 +0000");
        assert_eq!(git_date(1_700_000_000_000, 480), "Wed Nov 15 06:13:20 2023 +0800");
        assert_eq!(git_date(1_700_000_000_000, -330), "Tue Nov 14 16:43:20 2023 -0530");
        assert_eq!(git_date(-1_000, 0), "Wed Dec 31 23:59:59 1969 +0000");
    }

    #[test]
    fn format_time_known_values() {
        assert_eq!(format_time(0, 0), "1970-01-01 00:00 +0000");
        assert_eq!(format_time(1_700_000_000_000, 480), "2023-11-15 06:13 +0800");
        assert_eq!(format_time(1_700_000_000_000, -330), "2023-11-14 16:43 -0530");
        assert_eq!(format_time(951_782_400_000, 0), "2000-02-29 00:00 +0000", "leap day");
        assert_eq!(format_time(-1_000, 0), "1969-12-31 23:59 +0000", "before the epoch");
    }

    #[test]
    fn civil_from_days_boundaries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn diff_algorithm_choice() {
        let lines = |n: usize, tag: &str| (0..n).map(|i| format!("{tag} {i}\n")).collect::<String>().into_bytes();
        assert_eq!(diff_algorithm(b"a\nb\n", b"a\nc\n"), similar::Algorithm::Lcs);
        assert_eq!(diff_algorithm(b"", b""), similar::Algorithm::Lcs);
        // Big files that differ everywhere: the table would be too large
        assert_eq!(
            diff_algorithm(&lines(3000, "old"), &lines(3000, "new")),
            similar::Algorithm::Myers
        );
        // Big files with a small changed middle: only the middle counts
        let (mut a, mut b) = (lines(20_000, "same"), lines(20_000, "same"));
        a.extend(lines(100, "old"));
        b.extend(lines(120, "new"));
        a.extend(lines(20_000, "tail"));
        b.extend(lines(20_000, "tail"));
        assert_eq!(diff_algorithm(&a, &b), similar::Algorithm::Lcs);
    }

    #[test]
    fn render_patch_text_and_binary() {
        let (bin, p) = render_patch("a.txt", Some(b"one\ntwo\n"), Some(b"one\nthree\n"));
        assert!(!bin);
        let p = p.expect("patch");
        assert!(p.contains("--- a/a.txt") && p.contains("+++ b/a.txt"));
        assert!(p.contains("-two") && p.contains("+three"));
        let (bin, p) = render_patch("new.txt", None, Some(b"hi\n"));
        assert!(!bin && p.expect("patch").contains("+hi"));
        assert_eq!(render_patch("x.bin", Some(&[0, 1, 2]), Some(b"t")), (true, None));
        assert_eq!(render_patch("x.bin", None, Some(&[0xff, 0xfe])), (true, None));
    }

    #[test]
    fn status_and_paths() {
        assert_eq!(status_of(false, true), "A");
        assert_eq!(status_of(true, false), "D");
        assert_eq!(status_of(true, true), "M");
        assert!(in_path("src/a.rs", Some("src")));
        assert!(in_path("src", Some("src")));
        assert!(!in_path("srcx/a", Some("src")));
        assert!(in_path("any", None));
        assert!(in_path("any", Some("")));
        assert_eq!(mode_name(Mode::Executable), "executable");
        assert_eq!(mode_name(Mode::Symlink), "symlink");
        assert_eq!(mode_name(Mode::File), "file");
    }

    fn file(status: &'static str, old: Option<&'static str>, new: Option<&'static str>, binary: bool) -> DiffFile {
        DiffFile {
            path: "p".into(),
            status,
            old_id: None,
            new_id: None,
            old_mode: old,
            new_mode: new,
            binary,
            patch: Some("@@ -1 +1 @@\n".into()),
        }
    }

    #[test]
    fn diff_render_text() {
        let out = DiffOut {
            schema: "s",
            from: None,
            to: "t".into(),
            files: vec![],
            name_only: false,
        };
        assert_eq!(out.text(), "no differences");
        let out = DiffOut {
            files: vec![
                file("A", None, Some("file"), false),
                file("D", Some("file"), None, false),
                file("M", Some("file"), Some("executable"), false),
                file("M", Some("file"), Some("file"), true),
            ],
            ..out
        };
        let t = out.text();
        assert!(t.contains("new file (file)"));
        assert!(t.contains("deleted file (file)"));
        assert!(t.contains("mode change file => executable"));
        assert!(t.contains("Binary files a/p and b/p differ"));
        let names = DiffOut { name_only: true, ..out };
        assert_eq!(names.text(), "A p\nD p\nM p\nM p");
    }

    #[test]
    fn split_lines_matches_server() {
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], b"b"]);
        assert_eq!(split_lines(b"a\n"), vec![&b"a\n"[..]]);
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn blame_render_text() {
        let l = |n, author: &str| BlameLineOut {
            line: n,
            commit: "c1abcdefabcdef0".into(),
            author: author.into(),
            time_ms: 0,
            tz_offset_minutes: 0,
            content: "code".into(),
        };
        let out = BlameOut {
            schema: "s",
            revision: "main".into(),
            path: "f".into(),
            lines: vec![l(1, "Ada"), l(10, "A very long author name")],
        };
        let t = out.text();
        assert!(
            t.starts_with("c1abcdefabcd (Ada              1970-01-01)  1| code"),
            "{t}"
        );
        assert!(t.contains("(A very long auth 1970-01-01) 10| code"), "{t}");
    }

    #[test]
    fn log_render_text() {
        let line = |parents: usize| LogLine {
            id: "c1aaaaaaaaaaaaaaaa".into(),
            parents: vec!["c1p".into(); parents],
            author: "Ada".into(),
            email: "a@x".into(),
            time_ms: 0,
            tz_offset_minutes: 0,
            committer: "c".into(),
            committer_time_ms: 0,
            message: "subject\n\nbody".into(),
            generation: 1,
        };
        let out = LogOut {
            schema: "s",
            revision: "main".into(),
            path: None,
            entries: vec![line(1), line(2)],
            oneline: false,
        };
        assert_eq!(
            out.text(),
            "commit c1aaaaaaaaaaaaaaaa\nAuthor: Ada <a@x>\nDate:   Thu Jan 1 00:00:00 1970 +0000\n\n    subject\n    \n    body\n\n\
             commit c1aaaaaaaaaaaaaaaa\nMerge: c1p c1p\nAuthor: Ada <a@x>\nDate:   Thu Jan 1 00:00:00 1970 +0000\n\n    \
             subject\n    \n    body"
        );
        let one = LogOut {
            oneline: true,
            entries: vec![line(1)],
            ..out
        };
        assert_eq!(one.text(), "c1aaaaaaaaaa subject");
        let out = LogOut { oneline: false, ..one };
        let empty = LogOut { entries: vec![], ..out };
        assert_eq!(empty.text(), "");
        let bare = LogLine {
            message: String::new(),
            ..line(1)
        };
        assert!(commit_header(&bare).ends_with("+0000"));
    }
}
