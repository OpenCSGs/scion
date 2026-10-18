//! 稀疏检出的范围（客户端设计文档 §5.1）。
//!
//! 范围是一组目录：只检出根目录下的文件和这些目录下的全部内容。遍历目录树时不进入范围外的子树，
//! 它们的目录树和文件内容都不会被请求。提交只发送改动的路径，范围外的内容由服务端从父提交保留。

use crate::error::{Error, Result};

/// 检出范围。`dirs` 为 None 时检出全部；为空列表时只检出根目录下的文件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sparse {
    dirs: Option<Vec<String>>,
}

impl Sparse {
    /// 全部检出。
    pub fn full() -> Self {
        Self { dirs: None }
    }

    /// 只检出 `dirs`（以及根目录下的文件）。目录先规范化；被其它目录包含的目录去掉。
    pub fn dirs<S: AsRef<str>>(dirs: &[S]) -> Result<Self> {
        let mut out: Vec<String> = dirs.iter().map(|d| normalize(d.as_ref())).collect::<Result<_>>()?;
        out.sort();
        out.dedup();
        let kept = out
            .iter()
            .filter(|d| !out.iter().any(|o| under(d, o)))
            .cloned()
            .collect();
        Ok(Self { dirs: Some(kept) })
    }

    /// 由状态文件中保存的目录还原。
    pub(crate) fn from_state(dirs: Option<&[String]>) -> Result<Self> {
        match dirs {
            None => Ok(Self::full()),
            Some(d) => Self::dirs(d),
        }
    }

    pub fn is_full(&self) -> bool {
        self.dirs.is_none()
    }

    /// 范围内的目录；全部检出时为 None。
    pub fn list(&self) -> Option<&[String]> {
        self.dirs.as_deref()
    }

    /// 文件 `path`（`/` 分隔的相对路径）是否在范围内。
    pub fn includes_file(&self, path: &str) -> bool {
        match &self.dirs {
            None => true,
            Some(dirs) => !path.contains('/') || dirs.iter().any(|d| under(path, d)),
        }
    }

    /// 遍历目录树时是否进入目录 `dir`：它在某个范围目录之下，或是某个范围目录的上级。
    pub fn descends(&self, dir: &str) -> bool {
        match &self.dirs {
            None => true,
            Some(dirs) => dirs.iter().any(|d| d == dir || under(dir, d) || under(d, dir)),
        }
    }

    /// 加入目录。全部检出时没有意义，返回错误。
    pub fn add<S: AsRef<str>>(&self, dirs: &[S]) -> Result<Self> {
        let Some(cur) = &self.dirs else {
            return Err(Error::Sparse(
                "this is a full checkout; use `scn sparse set` to choose directories".into(),
            ));
        };
        let mut all = cur.clone();
        all.extend(dirs.iter().map(|d| d.as_ref().to_owned()));
        Self::dirs(&all)
    }

    /// 移除目录：必须正好是范围中的某一项。
    pub fn remove<S: AsRef<str>>(&self, dirs: &[S]) -> Result<Self> {
        let Some(cur) = &self.dirs else {
            return Err(Error::Sparse("this is a full checkout; nothing to remove".into()));
        };
        let mut left = cur.clone();
        for d in dirs {
            let d = normalize(d.as_ref())?;
            let Some(pos) = left.iter().position(|x| *x == d) else {
                return Err(Error::Sparse(match left.iter().find(|x| under(&d, x)) {
                    Some(parent) => format!("{d} is inside {parent}; remove {parent} instead"),
                    None => format!("{d} is not in the sparse checkout"),
                }));
            };
            left.remove(pos);
        }
        Ok(Self { dirs: Some(left) })
    }
}

/// `path` 是否在目录 `dir` 之下（按路径分段比较，`a/bc` 不在 `a/b` 之下）。
fn under(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
}

/// 规范化一个目录：去掉首尾的 `/` 与重复的 `/`；拒绝空目录、`.`、`..` 和 `.scion`。
fn normalize(dir: &str) -> Result<String> {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(Error::Sparse(format!("invalid sparse directory {dir:?}: empty")));
    }
    if let Some(bad) = parts.iter().find(|p| matches!(**p, "." | "..") || p.contains('\\')) {
        return Err(Error::Sparse(format!("invalid sparse directory {dir:?}: {bad:?}")));
    }
    if parts[0] == crate::state::DIR {
        return Err(Error::Sparse(format!("invalid sparse directory {dir:?}")));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_collapses_nested_dirs() {
        let s = Sparse::dirs(&["a/b/", "/c//d", "a/b/x", "a/b", "e"]).expect("dirs");
        assert_eq!(s.list(), Some(&["a/b".to_string(), "c/d".into(), "e".into()][..]));
        assert!(!s.is_full());
        for bad in ["", "/", "a/../b", "./a", ".scion", "a\\b"] {
            assert!(matches!(Sparse::dirs(&[bad]), Err(Error::Sparse(_))), "{bad}");
        }
        assert_eq!(Sparse::dirs::<&str>(&[]).expect("empty").list(), Some(&[][..]));
    }

    #[test]
    fn file_and_directory_matching() {
        let s = Sparse::dirs(&["a/b"]).expect("dirs");
        assert!(s.includes_file("top.txt"), "root files always");
        assert!(s.includes_file("a/b/c.txt"));
        assert!(s.includes_file("a/b/deep/c.txt"));
        assert!(!s.includes_file("a/x.txt"), "files in parent dirs are not included");
        assert!(!s.includes_file("a/bc/x.txt"), "component boundary");
        assert!(!s.includes_file("z/x.txt"));
        assert!(s.descends("a"));
        assert!(s.descends("a/b"));
        assert!(s.descends("a/b/deep"));
        assert!(!s.descends("a/bc"));
        assert!(!s.descends("z"));

        let full = Sparse::full();
        assert!(full.is_full() && full.list().is_none());
        assert!(full.includes_file("z/x.txt") && full.descends("z"));
        let none = Sparse::dirs::<&str>(&[]).expect("empty");
        assert!(none.includes_file("top") && !none.includes_file("a/x") && !none.descends("a"));
    }

    #[test]
    fn add_and_remove() {
        let s = Sparse::dirs(&["a/b"]).expect("dirs");
        assert_eq!(s.add(&["a"]).expect("add").list(), Some(&["a".to_string()][..]));
        assert_eq!(
            s.add(&["c"]).expect("add").list(),
            Some(&["a/b".to_string(), "c".into()][..])
        );
        assert_eq!(s.remove(&["a/b/"]).expect("rm").list(), Some(&[][..]));
        let wide = Sparse::dirs(&["a"]).expect("dirs");
        let e = wide.remove(&["a/b"]).expect_err("inside");
        assert!(e.to_string().contains("inside a"), "{e}");
        let e = s.remove(&["q"]).expect_err("absent");
        assert!(e.to_string().contains("not in the sparse checkout"), "{e}");
        assert!(s.remove(&[""]).is_err());
        assert!(Sparse::full().add(&["a"]).is_err());
        assert!(Sparse::full().remove(&["a"]).is_err());
    }

    #[test]
    fn restored_from_state() {
        assert!(Sparse::from_state(None).expect("full").is_full());
        let s = Sparse::from_state(Some(&["a/".to_string()])).expect("dirs");
        assert_eq!(s.list(), Some(&["a".to_string()][..]));
    }
}
