//! `git-upload-pack` (design §4): ref advertisement, negotiation, choosing objects and the pack.

use std::collections::{BinaryHeap, HashMap, HashSet};

use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt};
use scion_object::{EntryMode, ObjectId, Tree, TreeEntry};
use scion_query::{GITSHA_KIND, GITSHA_VERSION};
use scion_store::{BlobStore, ChangesetRecord, MetaStore};
use tokio::sync::mpsc;

use crate::objects::{Objects, check_blob};
use crate::pack::{Kind, PackWriter};
use crate::pkt::{self, MAX_BAND_DATA, Pkt, Reader};
use crate::{Error, Result};

/// The tag prefix of Scion ref names.
pub const TAG_PREFIX: &str = "tags/";
/// How many haves without a common commit end the negotiation: the client is then sent
/// everything its wants need.
const GIVE_UP_HAVES: usize = 256;
/// How many file contents are read at once while writing a pack.
const READ_AHEAD: usize = 16;

/// The git name of a Scion ref.
pub fn git_ref_name(name: &str) -> String {
    match name.strip_prefix(TAG_PREFIX) {
        Some(tag) => format!("refs/tags/{tag}"),
        None => format!("refs/heads/{name}"),
    }
}

/// The refs to advertise: sorted by git name, and the default branch if it exists.
pub struct Advertised {
    pub refs: Vec<(String, [u8; 20])>,
    /// `HEAD`: the default branch's git name and SHA.
    pub head: Option<(String, [u8; 20])>,
}

pub async fn advertised(meta: &dyn MetaStore, blobs: &dyn BlobStore, repo: &str, default: &str) -> Result<Advertised> {
    let refs = meta.list_refs(repo).await?;
    let mut objects = Objects::new(meta, blobs, repo);
    let targets: Vec<ObjectId> = refs.iter().map(|(_, s)| s.target).collect();
    objects.prefetch(&targets, &[]).await?;
    let mut out = Vec::with_capacity(refs.len());
    let mut head = None;
    for (name, state) in &refs {
        let sha = objects.commit_sha(state.target).await?;
        let git = git_ref_name(name);
        if name == default {
            head = Some((git.clone(), sha));
        }
        out.push((git, sha));
    }
    out.sort();
    Ok(Advertised { refs: out, head })
}

/// The v0 advertisement after the service line: refs with capabilities on the first line.
pub fn advertise_v0(adv: &Advertised, agent: &str) -> BytesMut {
    let mut caps =
        String::from("multi_ack_detailed no-done side-band-64k ofs-delta shallow deepen-since object-format=sha1");
    if let Some((target, _)) = &adv.head {
        caps.push_str(&format!(" symref=HEAD:{target}"));
    }
    caps.push_str(&format!(" agent={agent}"));
    let mut lines: Vec<(String, [u8; 20])> = Vec::new();
    if let Some((_, sha)) = &adv.head {
        lines.push(("HEAD".into(), *sha));
    }
    lines.extend(adv.refs.iter().cloned());
    let mut out = BytesMut::new();
    if lines.is_empty() {
        pkt::put(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", "0".repeat(40)).as_bytes(),
        );
    }
    for (i, (name, sha)) in lines.iter().enumerate() {
        let line = if i == 0 {
            format!("{} {name}\0{caps}\n", hex::encode(sha))
        } else {
            format!("{} {name}\n", hex::encode(sha))
        };
        pkt::put(&mut out, line.as_bytes());
    }
    out.extend_from_slice(pkt::FLUSH);
    out
}

/// The v2 capability advertisement.
pub fn advertise_v2(agent: &str) -> BytesMut {
    let mut out = BytesMut::new();
    for line in [
        "version 2",
        &format!("agent={agent}"),
        "ls-refs",
        "fetch=shallow",
        "object-format=sha1",
    ] {
        pkt::put_line(&mut out, line);
    }
    out.extend_from_slice(pkt::FLUSH);
    out
}

/// One v2 request: the command and its arguments.
pub struct V2Request<'a> {
    pub command: String,
    pub args: Vec<&'a [u8]>,
}

pub fn parse_v2(body: &[u8]) -> Result<V2Request<'_>> {
    let mut r = Reader::new(body);
    let mut command = None;
    // Capabilities until the delimiter, then arguments until the flush
    loop {
        match r.read()? {
            Some(Pkt::Data(l)) => {
                if let Some(c) = l.strip_prefix(b"command=") {
                    command = Some(String::from_utf8_lossy(c).into_owned());
                }
            }
            Some(Pkt::Delim) => break,
            Some(Pkt::Flush) | None => {
                return Ok(V2Request {
                    command: command.unwrap_or_default(),
                    args: Vec::new(),
                });
            }
            Some(Pkt::End) => return Err(Error::Protocol("unexpected response-end packet".into())),
        }
    }
    let mut args = Vec::new();
    while let Some(p) = r.read()? {
        match p {
            Pkt::Data(l) => args.push(l),
            Pkt::Flush => break,
            Pkt::Delim | Pkt::End => return Err(Error::Protocol("unexpected packet in arguments".into())),
        }
    }
    Ok(V2Request {
        command: command.ok_or_else(|| Error::Protocol("no command".into()))?,
        args,
    })
}

/// `ls-refs`: the refs matching the prefixes, with `HEAD`'s target when asked for.
pub fn ls_refs(adv: &Advertised, args: &[&[u8]]) -> BytesMut {
    let symrefs = args.contains(&&b"symrefs"[..]);
    let prefixes: Vec<&[u8]> = args.iter().filter_map(|a| a.strip_prefix(b"ref-prefix ")).collect();
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.as_bytes().starts_with(p));
    let mut out = BytesMut::new();
    if let Some((target, sha)) = adv.head.as_ref().filter(|_| wanted("HEAD")) {
        let mut line = format!("{} HEAD", hex::encode(sha));
        if symrefs {
            line.push_str(&format!(" symref-target:{target}"));
        }
        pkt::put_line(&mut out, &line);
    }
    for (name, sha) in &adv.refs {
        if wanted(name) {
            pkt::put_line(&mut out, &format!("{} {name}", hex::encode(sha)));
        }
    }
    out.extend_from_slice(pkt::FLUSH);
    out
}

/// What a fetch asks for, from either protocol version.
#[derive(Debug, Default)]
pub struct FetchRequest {
    pub wants: Vec<[u8; 20]>,
    pub haves: Vec<[u8; 20]>,
    pub done: bool,
    pub side_band: bool,
    pub no_progress: bool,
    /// v0: `multi_ack_detailed` and `no-done`.
    pub multi_ack: bool,
    pub no_done: bool,
    /// The client's shallow commits: it has them, but not their parents.
    pub shallow: Vec<[u8; 20]>,
    pub deepen: Option<Deepen>,
    /// v0: the request ended with its wants, before any haves. A shallow client asks this
    /// first, and expects only the shallow update back.
    pub wants_only: bool,
}

/// How deep a shallow fetch goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deepen {
    /// `--depth N`: the wants and N - 1 generations of parents.
    Depth(u32),
    /// `--shallow-since`: the commits committed at or after this time (seconds).
    Since(i64),
}

fn oid(s: &[u8]) -> Result<[u8; 20]> {
    let s = s.get(..40).ok_or_else(|| Error::Protocol("short object id".into()))?;
    let mut out = [0u8; 20];
    hex::decode_to_slice(s, &mut out).map_err(|_| Error::Protocol("bad object id".into()))?;
    Ok(out)
}

/// Refuses what this gateway does not do yet: partial clones, and deepening by excluded refs
/// or relative to the current boundary.
fn unsupported(line: &[u8]) -> Option<&'static str> {
    if line.starts_with(b"filter ") {
        Some("partial clones are not supported yet; clone without --filter")
    } else if line.starts_with(b"deepen-not ") {
        Some("--shallow-exclude is not supported; use --depth or --shallow-since")
    } else if line == b"deepen-relative" {
        Some("git fetch --deepen is not supported; use --depth or --unshallow")
    } else {
        None
    }
}

/// `shallow`, `deepen` and `deepen-since` lines, in either protocol version.
fn shallow_arg(req: &mut FetchRequest, line: &[u8]) -> Result<bool> {
    let number = |v: &[u8]| -> Result<i64> {
        std::str::from_utf8(v)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| Error::Protocol(format!("bad deepen {:?}", String::from_utf8_lossy(v))))
    };
    if let Some(s) = line.strip_prefix(b"shallow ") {
        req.shallow.push(oid(s)?);
    } else if let Some(n) = line.strip_prefix(b"deepen-since ") {
        req.deepen = Some(Deepen::Since(number(n)?));
    } else if let Some(n) = line.strip_prefix(b"deepen ") {
        let n = u32::try_from(number(n)?.max(1)).unwrap_or(u32::MAX);
        req.deepen = Some(Deepen::Depth(n));
    } else {
        return Ok(false);
    }
    Ok(true)
}

/// The arguments of a v2 `fetch`.
pub fn parse_fetch_v2(args: &[&[u8]]) -> Result<FetchRequest> {
    let mut req = FetchRequest {
        side_band: true,
        ..Default::default()
    };
    for a in args {
        if let Some(m) = unsupported(a) {
            return Err(Error::Unsupported(m.into()));
        }
        if shallow_arg(&mut req, a)? {
            continue;
        }
        if let Some(w) = a.strip_prefix(b"want ") {
            req.wants.push(oid(w)?);
        } else if let Some(h) = a.strip_prefix(b"have ") {
            req.haves.push(oid(h)?);
        } else if *a == b"done" {
            req.done = true;
        } else if *a == b"no-progress" {
            req.no_progress = true;
        }
    }
    Ok(req)
}

/// A v0 request: wants (the first with capabilities), a flush, haves, and `done` or a flush.
pub fn parse_fetch_v0(body: &[u8]) -> Result<FetchRequest> {
    let mut req = FetchRequest {
        wants_only: true,
        ..Default::default()
    };
    let mut r = Reader::new(body);
    let mut first = true;
    let mut flushes = 0;
    while let Some(p) = r.read()? {
        if flushes > 0 {
            req.wants_only = false;
        }
        let Pkt::Data(l) = p else {
            flushes += 1;
            continue;
        };
        if let Some(m) = unsupported(l) {
            return Err(Error::Unsupported(m.into()));
        }
        if shallow_arg(&mut req, l)? {
            continue;
        }
        if let Some(w) = l.strip_prefix(b"want ") {
            req.wants.push(oid(w)?);
            if first {
                let caps = String::from_utf8_lossy(w.get(40..).unwrap_or_default()).into_owned();
                for c in caps.split_whitespace() {
                    match c {
                        "side-band-64k" | "side-band" => req.side_band = true,
                        "no-progress" => req.no_progress = true,
                        "multi_ack_detailed" => req.multi_ack = true,
                        "no-done" => req.no_done = true,
                        _ => {}
                    }
                }
                first = false;
            }
        } else if let Some(h) = l.strip_prefix(b"have ") {
            req.haves.push(oid(h)?);
        } else if l == b"done" {
            req.done = true;
        }
    }
    Ok(req)
}

/// The haves that name commits of this repository, in request order.
pub async fn common(meta: &dyn MetaStore, repo: &str, haves: &[[u8; 20]]) -> Result<Vec<([u8; 20], ObjectId)>> {
    let mut out = Vec::new();
    for chunk in haves.chunks(1000) {
        let keys: Vec<Vec<u8>> = chunk.iter().map(|h| h.to_vec()).collect();
        let found = meta.get_derived_many(repo, GITSHA_KIND, GITSHA_VERSION, &keys).await?;
        for (sha, v) in chunk.iter().zip(found) {
            if let Some(id) = v.and_then(|v| ObjectId::from_bytes(&v).ok()) {
                out.push((*sha, id));
            }
        }
    }
    Ok(out)
}

/// Whether to stop negotiating and send the pack: a common commit was found, or the client has
/// nothing, or it has sent so many haves without one that it is unlikely to share history.
pub fn ready(req: &FetchRequest, common: usize) -> bool {
    common > 0 || req.haves.is_empty() || req.haves.len() >= GIVE_UP_HAVES
}

/// The wants' Scion commits; a want this repository does not have is an error.
pub async fn resolve_wants(meta: &dyn MetaStore, repo: &str, wants: &[[u8; 20]]) -> Result<Vec<ObjectId>> {
    let found = common(meta, repo, wants).await?;
    if found.len() != wants.len() {
        let known: HashSet<[u8; 20]> = found.iter().map(|(s, _)| *s).collect();
        let missing = wants.iter().find(|w| !known.contains(*w)).copied().unwrap_or_default();
        return Err(Error::Protocol(format!("not our ref {}", hex::encode(missing))));
    }
    Ok(found.into_iter().map(|(_, id)| id).collect())
}

/// What a pack holds: commits newest first, then trees, then files.
#[derive(Debug, Default)]
pub struct Plan {
    pub commits: Vec<ChangesetRecord>,
    pub trees: Vec<ObjectId>,
    pub files: Vec<ObjectId>,
}

impl Plan {
    pub fn len(&self) -> usize {
        self.commits.len() + self.trees.len() + self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

const WANTED: u8 = 1;
const HAVE: u8 = 2;

/// The commits reachable from `wants` and not from `haves`, newest first, taking the commits in
/// `cut` as having no parents (the shallow boundary). A walk in generation order: a commit is
/// popped only after every commit of a higher generation, so by then any path from a have has
/// marked it.
async fn commits(
    meta: &dyn MetaStore,
    repo: &str,
    wants: &[ObjectId],
    haves: &[ObjectId],
    cut: &HashSet<ObjectId>,
) -> Result<Vec<ChangesetRecord>> {
    let mut flags: HashMap<ObjectId, u8> = HashMap::new();
    let mut records: HashMap<ObjectId, ChangesetRecord> = HashMap::new();
    let mut queue: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
    // Queued commits still wanted: the walk ends when none is left
    let mut wanted = 0usize;
    for (ids, flag) in [(haves, HAVE), (wants, WANTED)] {
        for id in ids {
            let f = flags.entry(*id).or_insert(0);
            if *f == 0 {
                let rec = record(meta, repo, *id).await?;
                queue.push((rec.generation, *id));
                records.insert(*id, rec);
            }
            *f |= flag;
        }
    }
    wanted += flags.values().filter(|f| **f == WANTED).count();
    let mut out = Vec::new();
    while wanted > 0 {
        let Some((_, id)) = queue.pop() else { break };
        let f = flags[&id];
        if f == WANTED {
            wanted -= 1;
        }
        let rec = records.remove(&id).expect("queued commits have records");
        let parents: &[ObjectId] = if cut.contains(&id) { &[] } else { &rec.parents };
        for p in parents {
            let old = flags.get(p).copied();
            let new = old.unwrap_or(0) | if f & HAVE != 0 { HAVE } else { WANTED };
            if old == Some(new) {
                continue;
            }
            flags.insert(*p, new);
            match old {
                None => {
                    let prec = record(meta, repo, *p).await?;
                    queue.push((prec.generation, *p));
                    records.insert(*p, prec);
                    if new == WANTED {
                        wanted += 1;
                    }
                }
                // A wanted commit still queued turns out to be had
                Some(WANTED) => wanted -= 1,
                Some(_) => {}
            }
        }
        if f == WANTED {
            out.push(rec);
        }
    }
    Ok(out)
}

async fn record(meta: &dyn MetaStore, repo: &str, id: ObjectId) -> Result<ChangesetRecord> {
    meta.get_changeset(repo, &id)
        .await?
        .ok_or_else(|| Error::Internal(format!("commit {id} has no record")))
}

async fn load_tree(blobs: &dyn BlobStore, id: ObjectId) -> Result<Tree> {
    let bytes = blobs
        .get(&id)
        .await?
        .ok_or_else(|| Error::Internal(format!("tree {id} is missing")))?;
    Tree::decode(&bytes).map_err(|e| Error::Internal(format!("tree {id}: {e}")))
}

/// Chooses the objects to send (design §4). A tree or file whose id equals the entry at the
/// same path in one of the commit's parents is skipped: the parent is either sent too, and its
/// objects with it, or the client has it.
pub async fn plan(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    repo: &str,
    wants: &[ObjectId],
    haves: &[ObjectId],
    cut: &HashSet<ObjectId>,
) -> Result<Plan> {
    let commits = commits(meta, repo, wants, haves, cut).await?;
    let mut roots: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut plan = Plan::default();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for c in &commits {
        // A shallow commit's parents are not on the client: its whole tree goes
        let parents: &[ObjectId] = if cut.contains(&c.id) { &[] } else { &c.parents };
        let mut parent_trees = Vec::with_capacity(parents.len());
        for p in parents {
            let root = match roots.get(p) {
                Some(r) => *r,
                None => record(meta, repo, *p).await?.root_tree,
            };
            parent_trees.push(root);
        }
        roots.insert(c.id, c.root_tree);
        // An explicit stack of (tree, the same path's trees in the parents)
        let mut stack = vec![(c.root_tree, parent_trees)];
        while let Some((tree, bases)) = stack.pop() {
            if bases.contains(&tree) || !seen.insert(tree) {
                continue;
            }
            plan.trees.push(tree);
            let t = load_tree(blobs, tree).await?;
            let mut base_entries: Vec<HashMap<String, TreeEntry>> = Vec::with_capacity(bases.len());
            for b in &bases {
                let bt = load_tree(blobs, *b).await?;
                base_entries.push(bt.entries().iter().map(|e| (e.name.clone(), e.clone())).collect());
            }
            for e in t.entries() {
                let same: Vec<&TreeEntry> = base_entries.iter().filter_map(|m| m.get(&e.name)).collect();
                if same
                    .iter()
                    .any(|b| b.id == e.id && (b.mode == EntryMode::Tree) == (e.mode == EntryMode::Tree))
                {
                    continue;
                }
                if e.mode == EntryMode::Tree {
                    let sub: Vec<ObjectId> = same
                        .iter()
                        .filter(|b| b.mode == EntryMode::Tree)
                        .map(|b| b.id)
                        .collect();
                    stack.push((e.id, sub));
                } else if seen.insert(e.id) {
                    check_blob(e.id)?;
                    plan.files.push(e.id);
                }
            }
        }
    }
    plan.commits = commits;
    Ok(plan)
}

/// The shallow boundary of a fetch: the commits whose parents are not sent, and the
/// `shallow` and `unshallow` lines that tell the client.
#[derive(Debug, Default)]
pub struct Shallow {
    /// Commits whose parents the walk does not follow: the new boundary, and every shallow
    /// commit of the client (it has no parents of them, whatever the haves say).
    pub cut: HashSet<ObjectId>,
    /// The parents of client commits that stop being shallow: wanted too.
    pub more_wants: Vec<ObjectId>,
    pub shallow: Vec<[u8; 20]>,
    pub unshallow: Vec<[u8; 20]>,
}

/// Computes the boundary. A client's shallow commit that `deepen` reaches past is an
/// `unshallow` line, and its parents are wanted; a new boundary commit with parents is a
/// `shallow` line.
pub async fn shallow(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    repo: &str,
    wants: &[ObjectId],
    client: &[([u8; 20], ObjectId)],
    deepen: Option<Deepen>,
) -> Result<Shallow> {
    let client_ids: HashSet<ObjectId> = client.iter().map(|(_, id)| *id).collect();
    let Some(deepen) = deepen else {
        return Ok(Shallow {
            cut: client_ids,
            ..Default::default()
        });
    };
    // A walk from the wants, level by level: `inside` are the commits whose parents are sent
    let mut inside: HashSet<ObjectId> = HashSet::new();
    let mut boundary: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = wants.iter().copied().collect();
    let mut level: Vec<ObjectId> = wants.to_vec();
    let mut depth = 1u32;
    while !level.is_empty() {
        let mut next = Vec::new();
        for id in level {
            let rec = record(meta, repo, id).await?;
            let keep = match deepen {
                Deepen::Depth(n) => depth < n,
                Deepen::Since(t) => {
                    let mut all = true;
                    for p in &rec.parents {
                        let cs = scion_query::load_changeset(blobs, *p).await?;
                        all &= cs.committer.time_ms.div_euclid(1000) >= t;
                    }
                    all
                }
            };
            if !keep {
                if !rec.parents.is_empty() {
                    boundary.push(id);
                }
                continue;
            }
            inside.insert(id);
            for p in &rec.parents {
                if seen.insert(*p) {
                    next.push(*p);
                }
            }
        }
        level = next;
        depth = depth.saturating_add(1);
    }
    let mut objects = Objects::new(meta, blobs, repo);
    let mut out = Shallow::default();
    for id in boundary {
        out.cut.insert(id);
        if !client_ids.contains(&id) {
            out.shallow.push(objects.commit_sha(id).await?);
        }
    }
    for (sha, id) in client {
        out.cut.insert(*id);
        if inside.contains(id) {
            out.unshallow.push(*sha);
            out.more_wants.extend(record(meta, repo, *id).await?.parents);
        }
    }
    Ok(out)
}

/// Refuses a pack with redacted files: their git SHAs cover content nobody may read any more.
pub async fn check_redactions(meta: &dyn MetaStore, repo: &str, files: &[ObjectId]) -> Result<()> {
    for chunk in files.chunks(1000) {
        let found = meta.get_redactions(repo, chunk).await?;
        if let Some(r) = found.iter().find(|r| r.active) {
            return Err(Error::Unservable(format!(
                "file {} was redacted ({}); git cannot fetch history that contains it, use scn",
                r.id, r.redaction_id
            )));
        }
    }
    Ok(())
}

/// Sends pack data, on side band 1 when the client asked for side bands.
pub struct Out {
    tx: mpsc::Sender<Bytes>,
    side_band: bool,
    progress: bool,
}

impl Out {
    pub fn new(tx: mpsc::Sender<Bytes>, side_band: bool, progress: bool) -> Self {
        Self {
            tx,
            side_band,
            progress,
        }
    }

    /// Sends bytes as they are (`false` when the client went away).
    pub async fn raw(&self, b: Bytes) -> bool {
        self.tx.send(b).await.is_ok()
    }

    pub async fn data(&self, b: Bytes) -> bool {
        if !self.side_band {
            return self.raw(b).await;
        }
        for chunk in b.chunks(MAX_BAND_DATA) {
            if !self.raw(pkt::band(1, chunk)).await {
                return false;
            }
        }
        true
    }

    pub async fn progress(&self, msg: &str) -> bool {
        if !self.side_band || !self.progress {
            return true;
        }
        self.raw(pkt::band(2, msg.as_bytes())).await
    }

    /// Reports an error the client prints as `remote: …`; without side bands the pack just ends.
    pub async fn error(&self, msg: &str) {
        if self.side_band {
            let _ = self.raw(pkt::band(3, format!("{msg}\n").as_bytes())).await;
        }
    }

    pub async fn flush(&self) {
        if self.side_band {
            let _ = self.raw(Bytes::from_static(pkt::FLUSH)).await;
        }
    }
}

/// Writes the plan as a pack. Returns early, with no error, when the client goes away.
pub async fn write_pack(meta: &dyn MetaStore, blobs: &dyn BlobStore, repo: &str, plan: &Plan, out: &Out) -> Result<()> {
    let total = plan.len();
    if !out.progress(&format!("Counting objects: {total}, done.\n")).await {
        return Ok(());
    }
    let (mut w, head) = PackWriter::new(u32::try_from(total).map_err(|_| Error::Internal("too many objects".into()))?);
    if !out.data(head).await {
        return Ok(());
    }
    let mut objects = Objects::new(meta, blobs, repo);
    let ids: Vec<ObjectId> = plan.commits.iter().map(|c| c.id).collect();
    let roots: Vec<ObjectId> = plan.commits.iter().map(|c| c.root_tree).collect();
    objects.prefetch(&ids, &roots).await?;
    for c in &plan.commits {
        let body = objects.commit(c).await?;
        if !out.data(w.object(Kind::Commit, &body)).await {
            return Ok(());
        }
    }
    for t in &plan.trees {
        let tree = load_tree(blobs, *t).await?;
        let body = objects.tree(*t, &tree).await?;
        if !out.data(w.object(Kind::Tree, &body)).await {
            return Ok(());
        }
    }
    let files = futures::stream::iter(plan.files.iter().copied())
        .map(|id| async move {
            blobs
                .get(&id)
                .await?
                .ok_or_else(|| Error::Unservable(format!("the content of file {id} is missing (purged?)")))
        })
        .buffered(READ_AHEAD);
    let mut files = std::pin::pin!(files);
    let mut sent = 0usize;
    while let Some(content) = files.try_next().await? {
        if !out.data(w.object(Kind::Blob, &content)).await {
            return Ok(());
        }
        sent += 1;
        if sent % 10_000 == 0 && !out.progress(&format!("Writing files: {sent}\r")).await {
            return Ok(());
        }
    }
    out.data(w.finish()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(b: u8) -> [u8; 20] {
        [b; 20]
    }

    fn adv() -> Advertised {
        Advertised {
            refs: vec![("refs/heads/main".into(), sha(1)), ("refs/tags/v1".into(), sha(2))],
            head: Some(("refs/heads/main".into(), sha(1))),
        }
    }

    fn lines(b: &[u8]) -> Vec<String> {
        let mut r = Reader::new(b);
        let mut out = Vec::new();
        while let Some(p) = r.read().expect("pkt") {
            out.push(match p {
                Pkt::Data(d) => String::from_utf8_lossy(d).into_owned(),
                Pkt::Flush => "0000".into(),
                Pkt::Delim => "0001".into(),
                Pkt::End => "0002".into(),
            });
        }
        out
    }

    #[test]
    fn names_refs_for_git() {
        assert_eq!(git_ref_name("main"), "refs/heads/main");
        assert_eq!(git_ref_name("feature/x"), "refs/heads/feature/x");
        assert_eq!(git_ref_name("tags/v1"), "refs/tags/v1");
    }

    #[test]
    fn advertises_v0_refs_and_an_empty_repository() {
        let got = lines(&advertise_v0(&adv(), "scion/1"));
        assert!(got[0].starts_with(&format!("{} HEAD\0multi_ack_detailed", "01".repeat(20))));
        assert!(got[0].ends_with("symref=HEAD:refs/heads/main agent=scion/1"));
        assert_eq!(got[1], format!("{} refs/heads/main", "01".repeat(20)));
        assert_eq!(got[2], format!("{} refs/tags/v1", "02".repeat(20)));
        assert_eq!(got[3], "0000");
        let empty = Advertised {
            refs: vec![],
            head: None,
        };
        let got = lines(&advertise_v0(&empty, "scion/1"));
        assert!(got[0].starts_with(&format!("{} capabilities^{{}}\0", "0".repeat(40))));
        assert!(!got[0].contains("symref"));
        assert_eq!(lines(&advertise_v2("scion/1"))[0], "version 2");
    }

    #[test]
    fn lists_refs_by_prefix() {
        let all = lines(&ls_refs(&adv(), &[b"symrefs"]));
        assert_eq!(
            all[0],
            format!("{} HEAD symref-target:refs/heads/main", "01".repeat(20))
        );
        assert_eq!(all.len(), 4);
        let tags = lines(&ls_refs(&adv(), &[b"ref-prefix refs/tags/"]));
        assert_eq!(tags, [format!("{} refs/tags/v1", "02".repeat(20)), "0000".into()]);
        let head = lines(&ls_refs(&adv(), &[b"ref-prefix HEAD"]));
        assert_eq!(head, [format!("{} HEAD", "01".repeat(20)), "0000".into()]);
    }

    #[test]
    fn parses_v2_requests() {
        let mut body = BytesMut::new();
        pkt::put_line(&mut body, "command=fetch");
        pkt::put_line(&mut body, "agent=git/2");
        body.extend_from_slice(pkt::DELIM);
        pkt::put_line(&mut body, &format!("want {}", "ab".repeat(20)));
        pkt::put_line(&mut body, &format!("have {}", "cd".repeat(20)));
        pkt::put_line(&mut body, "no-progress");
        pkt::put_line(&mut body, "done");
        body.extend_from_slice(pkt::FLUSH);
        let req = parse_v2(&body).expect("parse");
        assert_eq!(req.command, "fetch");
        let f = parse_fetch_v2(&req.args).expect("fetch");
        assert_eq!(f.wants, [[0xab; 20]]);
        assert_eq!(f.haves, [[0xcd; 20]]);
        assert!(f.done && f.no_progress && f.side_band);

        // A command with no arguments; no command at all; a stray packet
        let mut b = BytesMut::new();
        pkt::put_line(&mut b, "command=ls-refs");
        b.extend_from_slice(pkt::FLUSH);
        let r = parse_v2(&b).expect("parse");
        assert_eq!((r.command.as_str(), r.args.len()), ("ls-refs", 0));
        assert!(parse_v2(b"00010000").is_err());
        assert!(parse_v2(b"0002").is_err());
        let mut b = BytesMut::new();
        pkt::put_line(&mut b, "command=fetch");
        b.extend_from_slice(pkt::DELIM);
        b.extend_from_slice(pkt::DELIM);
        assert!(parse_v2(&b).is_err());
    }

    #[test]
    fn parses_v0_requests() {
        let mut body = BytesMut::new();
        pkt::put_line(
            &mut body,
            &format!(
                "want {} multi_ack_detailed side-band-64k no-done no-progress ofs-delta",
                "ab".repeat(20)
            ),
        );
        pkt::put_line(&mut body, &format!("want {}", "ac".repeat(20)));
        body.extend_from_slice(pkt::FLUSH);
        pkt::put_line(&mut body, &format!("have {}", "cd".repeat(20)));
        pkt::put_line(&mut body, "done");
        let f = parse_fetch_v0(&body).expect("parse");
        assert_eq!(f.wants, [[0xab; 20], [0xac; 20]]);
        assert_eq!(f.haves, [[0xcd; 20]]);
        assert!(f.done && f.side_band && f.no_progress && f.multi_ack && f.no_done);
    }

    #[test]
    fn parses_shallow_requests() {
        let shallow = format!("shallow {}", "ab".repeat(20));
        let f = parse_fetch_v2(&[shallow.as_bytes(), b"deepen 3"]).expect("parse");
        assert_eq!(
            (f.shallow.clone(), f.deepen),
            (vec![[0xab; 20]], Some(Deepen::Depth(3)))
        );
        let f = parse_fetch_v2(&[b"deepen-since 1700000000"]).expect("parse");
        assert_eq!(f.deepen, Some(Deepen::Since(1_700_000_000)));
        // --unshallow asks for the largest depth; 0 means 1
        let f = parse_fetch_v2(&[b"deepen 2147483647"]).expect("parse");
        assert_eq!(f.deepen, Some(Deepen::Depth(2_147_483_647)));
        assert_eq!(
            parse_fetch_v2(&[b"deepen 0"]).expect("parse").deepen,
            Some(Deepen::Depth(1))
        );
        let mut b = BytesMut::new();
        pkt::put_line(&mut b, &format!("want {} shallow", "cd".repeat(20)));
        pkt::put_line(&mut b, &shallow);
        pkt::put_line(&mut b, "deepen 1");
        let f = parse_fetch_v0(&b).expect("parse");
        assert_eq!((f.shallow.len(), f.deepen), (1, Some(Deepen::Depth(1))));
        assert!(parse_fetch_v2(&[b"deepen soon"]).is_err());
        assert!(parse_fetch_v2(&[b"shallow abc"]).is_err());
    }

    #[test]
    fn refuses_partial_clones_and_bad_ids() {
        for line in ["deepen-not main", "deepen-relative", "filter blob:none"] {
            let e = parse_fetch_v2(&[line.as_bytes()]).expect_err(line);
            assert!(matches!(e, Error::Unsupported(_)), "{line}");
            let mut b = BytesMut::new();
            pkt::put_line(&mut b, line);
            assert!(parse_fetch_v0(&b).is_err(), "{line}");
        }
        assert!(parse_fetch_v2(&[b"want abc"]).is_err());
        assert!(parse_fetch_v2(&[format!("want {}", "zz".repeat(20)).as_bytes()]).is_err());
    }

    #[test]
    fn decides_when_to_send_the_pack() {
        let mut req = FetchRequest::default();
        assert!(ready(&req, 0));
        req.haves = vec![sha(1); 3];
        assert!(!ready(&req, 0));
        assert!(ready(&req, 1));
        req.haves = vec![sha(1); GIVE_UP_HAVES];
        assert!(ready(&req, 0));
        let p = Plan::default();
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn refuses_redacted_files_and_unknown_wants() {
        let s = scion_store::MemStore::new();
        s.create_repo("r").await.expect("repo");
        let id = ObjectId::of_blob(b"secret");
        assert_eq!(check_redactions(&s, "r", &[id]).await, Ok(()));
        s.put_redactions(&[scion_store::Redaction {
            id,
            repo: "r".into(),
            redaction_id: "red-1".into(),
            reason: String::new(),
            created_by: "root".into(),
            created_at_ms: 0,
            active: true,
            purge_after_ms: None,
            purged_at_ms: None,
            seq: 0,
        }])
        .await
        .expect("redact");
        let e = check_redactions(&s, "r", &[id]).await.expect_err("redacted");
        assert!(e.to_string().contains("red-1"), "{e}");
        let e = resolve_wants(&s, "r", &[sha(7)]).await.expect_err("unknown");
        assert!(e.to_string().contains(&"07".repeat(20)), "{e}");
    }
}
