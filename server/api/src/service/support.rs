//! 各服务共用的小工具。

use scion_object::{ObjectId, ObjectKind};
use scion_store::{ChangesetRecord, MetaStore};
use tonic::Status;

use crate::status;

pub(crate) fn parse_id(raw: &[u8]) -> Result<ObjectId, Status> {
    ObjectId::from_bytes(raw).map_err(|e| status::invalid_object_id(raw, &e))
}

pub(crate) async fn check_repo(meta: &dyn MetaStore, repo: &str) -> Result<(), Status> {
    match meta.repo_exists(repo).await.map_err(status::from_store)? {
        true => Ok(()),
        false => Err(status::repo_not_found(repo)),
    }
}

/// One git-style navigation step after a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nav {
    /// `~N`: the N-th first-parent ancestor.
    Ancestor(u32),
    /// `^N`: the N-th parent (`^0` is the commit itself).
    Parent(u32),
}

/// Splits `rev` into a base and its git-style suffix (`main~2`, `c1…^2`, `main^^`). None when
/// there is no suffix, the base is empty, or the suffix is malformed.
pub(crate) fn split_nav(rev: &str) -> Option<(&str, Vec<Nav>)> {
    let start = rev.find(['~', '^'])?;
    if start == 0 {
        return None;
    }
    let (base, mut rest) = rev.split_at(start);
    let mut steps = Vec::new();
    while let Some(op) = rest.chars().next() {
        rest = &rest[1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n = match digits {
            0 => 1,
            _ => rest[..digits].parse().ok()?,
        };
        rest = &rest[digits..];
        steps.push(match op {
            '~' => Nav::Ancestor(n),
            '^' => Nav::Parent(n),
            _ => return None,
        });
    }
    Some((base, steps))
}

/// 把 revision（ref 名，或提交 id 的文本形式 `c1…`）解析为提交记录。
///
/// Also accepts a 40-hex git SHA, and git-style suffixes: `REV~N` (N-th first-parent ancestor),
/// `REV^` / `REV^N` (N-th parent), combinable as in git (`main~2^2`). An existing ref whose
/// name contains `~` or `^` wins over the suffix reading.
pub(crate) async fn resolve_revision(
    meta: &dyn MetaStore,
    repo: &str,
    revision: &str,
) -> Result<ChangesetRecord, Status> {
    check_repo(meta, repo).await?;
    if let Some(rec) = resolve_exact(meta, repo, revision).await? {
        return Ok(rec);
    }
    let Some((base, steps)) = split_nav(revision) else {
        return Err(status::ref_not_found(revision));
    };
    let mut rec = resolve_exact(meta, repo, base)
        .await?
        .ok_or_else(|| status::ref_not_found(revision))?;
    for step in steps {
        let next = match step {
            Nav::Ancestor(n) => {
                let mut id = rec.id;
                for _ in 0..n {
                    id = *meta
                        .get_changeset(repo, &id)
                        .await
                        .map_err(status::from_store)?
                        .ok_or_else(|| status::ref_not_found(revision))?
                        .parents
                        .first()
                        .ok_or_else(|| status::ref_not_found(revision))?;
                }
                id
            }
            Nav::Parent(0) => rec.id,
            Nav::Parent(n) => *rec
                .parents
                .get(n as usize - 1)
                .ok_or_else(|| status::ref_not_found(revision))?,
        };
        rec = meta
            .get_changeset(repo, &next)
            .await
            .map_err(status::from_store)?
            .ok_or_else(|| status::ref_not_found(revision))?;
    }
    Ok(rec)
}

/// A revision without suffixes: a changeset id, a ref name, or a git SHA.
async fn resolve_exact(meta: &dyn MetaStore, repo: &str, revision: &str) -> Result<Option<ChangesetRecord>, Status> {
    let cs_id = match revision.parse::<ObjectId>() {
        Ok(id) if id.kind() == ObjectKind::Changeset => id,
        _ => match meta.get_ref(repo, revision).await.map_err(status::from_store)? {
            Some(r) => r.target,
            // 40 位 git sha
            None => match scion_query::parse_git_sha(revision) {
                Some(sha) => match scion_query::lookup_git_sha(meta, repo, &sha)
                    .await
                    .map_err(crate::service::history::from_query)?
                {
                    Some(id) => id,
                    None => return Ok(None),
                },
                None => return Ok(None),
            },
        },
    };
    meta.get_changeset(repo, &cs_id).await.map_err(status::from_store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_store::{MemStore, RefExpect};
    use tonic::Code;

    fn cs(n: u8) -> ObjectId {
        ObjectId::hash(ObjectKind::Changeset, &[n])
    }

    #[test]
    fn parse_id_cases() {
        let id = ObjectId::of_blob(b"x");
        assert_eq!(parse_id(&id.to_bytes()).ok(), Some(id));
        assert_eq!(
            parse_id(b"short").map_err(|s| s.code()).err(),
            Some(Code::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn check_repo_cases() {
        let s = MemStore::new();
        s.create_repo("r").await.expect("repo");
        assert!(check_repo(&s, "r").await.is_ok());
        assert_eq!(
            check_repo(&s, "x").await.map_err(|s| s.code()).err(),
            Some(Code::NotFound)
        );
    }

    #[test]
    fn split_nav_cases() {
        use Nav::*;
        assert_eq!(split_nav("main~2"), Some(("main", vec![Ancestor(2)])));
        assert_eq!(split_nav("main^"), Some(("main", vec![Parent(1)])));
        assert_eq!(
            split_nav("main^^~"),
            Some(("main", vec![Parent(1), Parent(1), Ancestor(1)]))
        );
        assert_eq!(
            split_nav("a/b~10^2^0"),
            Some(("a/b", vec![Ancestor(10), Parent(2), Parent(0)]))
        );
        assert_eq!(split_nav("main"), None);
        assert_eq!(split_nav("~1"), None, "no base");
        assert_eq!(split_nav("main~x"), None, "junk after the suffix");
        assert_eq!(split_nav("main~99999999999"), None, "out of range");
    }

    #[tokio::test]
    async fn resolve_revision_with_git_suffixes() {
        let s = MemStore::new();
        s.create_repo("r").await.expect("repo");
        let tree = ObjectId::hash(ObjectKind::Tree, b"t");
        // 1 ─ 2 ─ 4   (4 merges 3)
        //  \─ 3 ─╯
        for (n, parents) in [
            (1u8, vec![]),
            (2, vec![cs(1)]),
            (3, vec![cs(1)]),
            (4, vec![cs(2), cs(3)]),
        ] {
            let rec = ChangesetRecord {
                id: cs(n),
                root_tree: tree,
                generation: u64::from(n),
                parents,
                fp_depth: 0,
                fp_jump: None,
            };
            s.put_changeset("r", &rec).await.expect("put");
        }
        s.cas_ref("r", "main", RefExpect::Absent, cs(4)).await.expect("ref");
        let id = |rev: &'static str| {
            let s = &s;
            async move { resolve_revision(s, "r", rev).await.map(|r| r.id).map_err(|e| e.code()) }
        };
        assert_eq!(id("main~1").await, Ok(cs(2)));
        assert_eq!(id("main^").await, Ok(cs(2)));
        assert_eq!(id("main^2").await, Ok(cs(3)));
        assert_eq!(id("main^2~1").await, Ok(cs(1)));
        assert_eq!(id("main~2").await, Ok(cs(1)));
        assert_eq!(id("main^0").await, Ok(cs(4)));
        assert_eq!(id("main~0").await, Ok(cs(4)));
        assert_eq!(id("main~3").await, Err(Code::NotFound), "past the root");
        assert_eq!(id("main^3").await, Err(Code::NotFound), "no third parent");
        assert_eq!(id("nope~1").await, Err(Code::NotFound));
        let by_id = format!("{}^2", cs(4));
        assert_eq!(resolve_revision(&s, "r", &by_id).await.map(|r| r.id).ok(), Some(cs(3)));
        // A ref literally named like a suffix wins
        s.cas_ref("r", "main~1", RefExpect::Absent, cs(3)).await.expect("ref");
        assert_eq!(id("main~1").await, Ok(cs(3)));
        // A dangling base
        s.cas_ref("r", "dangling", RefExpect::Absent, cs(9)).await.expect("ref");
        assert_eq!(id("dangling~1").await, Err(Code::NotFound));
        // A missing ancestor record on the way
        s.put_changeset(
            "r",
            &ChangesetRecord {
                id: cs(5),
                root_tree: tree,
                parents: vec![cs(8)],
                generation: 9,
                fp_depth: 0,
                fp_jump: None,
            },
        )
        .await
        .expect("put");
        s.cas_ref("r", "broken", RefExpect::Absent, cs(5)).await.expect("ref");
        assert_eq!(id("broken~1").await, Err(Code::NotFound));
        assert_eq!(id("broken~2").await, Err(Code::NotFound));
    }

    #[tokio::test]
    async fn resolve_revision_by_ref_and_id() {
        let s = MemStore::new();
        s.create_repo("r").await.expect("repo");
        let tree = ObjectId::hash(ObjectKind::Tree, b"t");
        let rec = ChangesetRecord {
            id: cs(1),
            root_tree: tree,
            parents: vec![],
            generation: 1,
            fp_depth: 0,
            fp_jump: None,
        };
        s.put_changeset("r", &rec).await.expect("put");
        s.cas_ref("r", "main", RefExpect::Absent, cs(1)).await.expect("ref");
        assert_eq!(resolve_revision(&s, "r", "main").await.ok(), Some(rec.clone()));
        assert_eq!(resolve_revision(&s, "r", &cs(1).to_string()).await.ok(), Some(rec));
        let code = |r: Result<ChangesetRecord, Status>| r.map_err(|s| s.code()).err();
        assert_eq!(code(resolve_revision(&s, "r", "nope").await), Some(Code::NotFound));
        assert_eq!(
            code(resolve_revision(&s, "r", &cs(9).to_string()).await),
            Some(Code::NotFound)
        );
        assert_eq!(code(resolve_revision(&s, "x", "main").await), Some(Code::NotFound));
        // 指向未记录提交的 ref（不应出现，但要报错而不是崩溃）
        s.cas_ref("r", "dangling", RefExpect::Absent, cs(7)).await.expect("ref");
        assert_eq!(code(resolve_revision(&s, "r", "dangling").await), Some(Code::NotFound));
        // 40 位 git sha
        let sha = [5u8; 20];
        s.put_derived(
            "r",
            scion_query::GITSHA_KIND,
            scion_query::GITSHA_VERSION,
            &sha,
            &cs(1).to_bytes(),
        )
        .await
        .expect("map");
        assert_eq!(
            resolve_revision(&s, "r", &hex::encode(sha)).await.ok().map(|r| r.id),
            Some(cs(1))
        );
        assert_eq!(
            code(resolve_revision(&s, "r", &hex::encode([6u8; 20])).await),
            Some(Code::NotFound)
        );
        // 不是提交 id 的 ObjectId 文本按 ref 名处理
        let blob = ObjectId::of_blob(b"x").to_string();
        assert_eq!(code(resolve_revision(&s, "r", &blob).await), Some(Code::NotFound));
    }
}
