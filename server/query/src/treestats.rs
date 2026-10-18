//! TreeStats 派生数据：目录树下的文件数、子目录数与总字节数（仓库大小、文件数）。
//!
//! 只由目录树内容决定，按目录树 id 缓存；未变化的子目录 id 相同，新提交只需计算改动路径上的目录。

use std::future::Future;
use std::pin::Pin;

use scion_object::{EntryMode, ObjectId, Tree};
use scion_store::{BlobStore, MetaStore};

use crate::{QueryError, Result};

pub const TREE_STATS_KIND: &str = "tree-stats";
pub const TREE_STATS_VERSION: u32 = 1;

/// 目录树（含全部子目录）的统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// 文件数（含可执行文件与符号链接）。
    pub files: u64,
    /// 子目录数（不含自身）。
    pub dirs: u64,
    /// 文件字节数之和。
    pub bytes: u64,
}

impl TreeStats {
    fn encode(&self) -> Vec<u8> {
        [self.files, self.dirs, self.bytes]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != 24 {
            return None;
        }
        let at = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or_default());
        Some(Self {
            files: at(0),
            dirs: at(1),
            bytes: at(2),
        })
    }
}

type StatsFuture<'a> = Pin<Box<dyn Future<Output = Result<TreeStats>> + Send + 'a>>;

/// 目录树 `id` 的统计（递归，深度等于目录层数）。
pub fn tree_stats<'a>(
    meta: &'a dyn MetaStore,
    blobs: &'a dyn BlobStore,
    repo: &'a str,
    id: ObjectId,
) -> StatsFuture<'a> {
    Box::pin(async move {
        if let Some(hit) = meta
            .get_derived(repo, TREE_STATS_KIND, TREE_STATS_VERSION, &id.to_bytes())
            .await?
            .and_then(|v| TreeStats::decode(&v))
        {
            return Ok(hit);
        }
        let bytes = blobs.get(&id).await?.ok_or(QueryError::MissingObject(id))?;
        let tree = Tree::decode(&bytes).map_err(|e| QueryError::Corrupt(id, e.to_string()))?;
        // 子目录的缓存一次批量读取，只对未命中的递归计算
        let subdirs: Vec<ObjectId> = tree
            .entries()
            .iter()
            .filter(|e| e.mode == EntryMode::Tree)
            .map(|e| e.id)
            .collect();
        let keys: Vec<Vec<u8>> = subdirs.iter().map(|id| id.to_bytes().to_vec()).collect();
        let known = meta
            .get_derived_many(repo, TREE_STATS_KIND, TREE_STATS_VERSION, &keys)
            .await?;
        let mut out = TreeStats::default();
        for (sub_id, hit) in subdirs.iter().zip(known) {
            let sub = match hit.as_deref().and_then(TreeStats::decode) {
                Some(sub) => sub,
                None => tree_stats(meta, blobs, repo, *sub_id).await?,
            };
            out.files += sub.files;
            out.dirs += sub.dirs + 1;
            out.bytes += sub.bytes;
        }
        for e in tree.entries().iter().filter(|e| e.mode != EntryMode::Tree) {
            out.files += 1;
            out.bytes += e.size;
        }
        meta.put_derived(repo, TREE_STATS_KIND, TREE_STATS_VERSION, &id.to_bytes(), &out.encode())
            .await?;
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use scion_object::TreeEntry;
    use scion_store::MemStore;

    async fn put_tree(s: &MemStore, entries: Vec<TreeEntry>) -> ObjectId {
        let bytes = Tree::new(entries).expect("tree").encode();
        let id = ObjectId::hash(scion_object::ObjectKind::Tree, &bytes);
        s.put(id, Bytes::from(bytes)).await.expect("put");
        id
    }

    fn file(name: &str, size: u64) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            mode: EntryMode::File,
            id: ObjectId::of_blob(name.as_bytes()),
            size,
        }
    }

    fn dir(name: &str, id: ObjectId) -> TreeEntry {
        TreeEntry {
            name: name.into(),
            mode: EntryMode::Tree,
            id,
            size: 0,
        }
    }

    #[tokio::test]
    async fn counts_recursively_and_caches() {
        let s = MemStore::new();
        s.create_repo("r").await.expect("repo");
        let empty = put_tree(&s, vec![]).await;
        let sub = put_tree(&s, vec![file("a", 10), dir("e", empty)]).await;
        let root = put_tree(&s, vec![file("b", 5), dir("s", sub), dir("t", sub)]).await;
        let want = TreeStats {
            files: 3,
            dirs: 4,
            bytes: 25,
        };
        assert_eq!(tree_stats(&s, &s, "r", root).await, Ok(want));
        assert_eq!(tree_stats(&s, &s, "r", empty).await, Ok(TreeStats::default()));
        // 缓存命中：删掉对象后仍能得到结果
        s.delete(&root).await.expect("delete");
        assert_eq!(tree_stats(&s, &s, "r", root).await, Ok(want));
        // 损坏的缓存值被忽略
        let other = put_tree(&s, vec![file("z", 1)]).await;
        s.put_derived("r", TREE_STATS_KIND, TREE_STATS_VERSION, &other.to_bytes(), b"bad")
            .await
            .expect("put");
        assert_eq!(tree_stats(&s, &s, "r", other).await.map(|t| t.files), Ok(1));
    }

    #[tokio::test]
    async fn missing_and_corrupt_trees() {
        let s = MemStore::new();
        s.create_repo("r").await.expect("repo");
        let gone = ObjectId::hash(scion_object::ObjectKind::Tree, b"gone");
        assert_eq!(
            tree_stats(&s, &s, "r", gone).await,
            Err(QueryError::MissingObject(gone))
        );
        let junk = ObjectId::hash(scion_object::ObjectKind::Tree, b"junk");
        s.put(junk, Bytes::from_static(b"junk")).await.expect("put");
        assert!(matches!(tree_stats(&s, &s, "r", junk).await, Err(QueryError::Corrupt(id, _)) if id == junk));
    }
}
