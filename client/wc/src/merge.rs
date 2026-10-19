//! Three-way merge of two snapshots, path by path, for `scn merge`.
//!
//! The rules match the server's rebase (`scion-merge`): a path changed on one side only takes
//! that side; when both sides changed a regular or executable file, its lines are merged with
//! diff3 and its mode with the same three-way rule. These are conflicts: binary files, symlinks,
//! files larger than [`MAX_MERGE_BYTES`], modify/delete, both sides adding different content at
//! one path, and a file on one side where the other has a directory.

use std::collections::{BTreeMap, BTreeSet};

use crate::TreeFile;

/// Files larger than this are not merged line by line.
pub const MAX_MERGE_BYTES: usize = 8 << 20;

/// What a merged path ends up as, relative to ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// The file from one side (or a three-way merged mode on an unchanged id); None deletes it.
    Take(Option<TreeFile>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    Content,
    AddAdd,
    ModifyDelete,
    DeleteModify,
    Binary,
    TooLarge,
    Symlink,
    Path,
}

impl ConflictKind {
    /// Name used in state files, JSON output and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::AddAdd => "add_add",
            Self::ModifyDelete => "modify_delete",
            Self::DeleteModify => "delete_modify",
            Self::Binary => "binary",
            Self::TooLarge => "too_large",
            Self::Symlink => "symlink",
            Self::Path => "path",
        }
    }
}

/// Whether `data` still has conflict markers: a line starting with `<<<<<<<` and one with `>>>>>>>`.
pub fn has_markers(data: &[u8]) -> bool {
    let starts = |m: &[u8]| data.split(|b| *b == b'\n').any(|l| l.starts_with(m));
    starts(b"<<<<<<<") && starts(b">>>>>>>")
}

/// A path both sides changed in incompatible ways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    pub path: String,
    pub kind: ConflictKind,
}

/// The pure part of a path's merge: either a decision or "needs the file contents".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Same,
    Done(Resolution),
    Conflict(ConflictKind),
    /// Both sides changed the content of a regular file: merge lines, with this mode.
    Lines {
        mode: crate::Mode,
    },
}

fn merge3<T: PartialEq + Copy>(base: T, ours: T, theirs: T) -> Option<T> {
    if ours == base {
        Some(theirs)
    } else if theirs == base || ours == theirs {
        Some(ours)
    } else {
        None
    }
}

pub(crate) fn step(b: Option<TreeFile>, o: Option<TreeFile>, t: Option<TreeFile>) -> Step {
    if o == t || t == b {
        return Step::Same;
    }
    if o == b {
        return Step::Done(Resolution::Take(t));
    }
    let (b, o, t) = match (b, o, t) {
        (_, None, Some(_)) => return Step::Conflict(ConflictKind::DeleteModify),
        (_, Some(_), None) => return Step::Conflict(ConflictKind::ModifyDelete),
        (None, Some(_), Some(_)) => return Step::Conflict(ConflictKind::AddAdd),
        (Some(b), Some(o), Some(t)) => (b, o, t),
        // o == t covers (None, None)
        (_, None, None) => return Step::Same,
    };
    if [b.mode, o.mode, t.mode].contains(&crate::Mode::Symlink) {
        return Step::Conflict(ConflictKind::Symlink);
    }
    // Only regular and executable remain: if both changed the mode they changed it the same way
    let mode = merge3(b.mode, o.mode, t.mode).unwrap_or(o.mode);
    match merge3(b.id, o.id, t.id) {
        Some(id) if id == o.id && mode == o.mode => Step::Same,
        Some(id) => Step::Done(Resolution::Take(Some(TreeFile { mode, id }))),
        None => Step::Lines { mode },
    }
}

/// Same heuristic as git: a NUL in the first 8000 bytes means binary.
fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

/// Line-merges one file. On a content conflict returns the text with conflict markers.
pub(crate) fn merge_lines(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Vec<u8>, (ConflictKind, Option<Vec<u8>>)> {
    if [base, ours, theirs].iter().any(|d| d.len() > MAX_MERGE_BYTES) {
        return Err((ConflictKind::TooLarge, None));
    }
    if [base, ours, theirs].iter().any(|d| is_binary(d)) {
        return Err((ConflictKind::Binary, None));
    }
    diffy::merge_bytes(base, ours, theirs).map_err(|marked| (ConflictKind::Content, Some(marked)))
}

/// Paths in `files` that are also a parent directory of another path: a file on one side and
/// a directory on the other cannot both exist.
pub(crate) fn path_clashes(files: &BTreeSet<String>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for p in files {
        let mut dir = p.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            if files.contains(parent) {
                out.insert(parent.to_owned());
                out.insert(p.clone());
            }
            dir = parent;
        }
    }
    out.into_iter().collect()
}

/// Union of the paths of three snapshots.
pub(crate) fn all_paths(maps: [&BTreeMap<String, TreeFile>; 3]) -> BTreeSet<&String> {
    maps.iter().flat_map(|m| m.keys()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mode;
    use scion_object::ObjectId;

    fn f(content: &str) -> Option<TreeFile> {
        Some(TreeFile {
            mode: Mode::File,
            id: ObjectId::of_blob(content.as_bytes()),
        })
    }

    fn with_mode(mode: Mode, content: &str) -> Option<TreeFile> {
        Some(TreeFile {
            mode,
            id: ObjectId::of_blob(content.as_bytes()),
        })
    }

    #[test]
    fn step_rules() {
        let (a, b, c) = (f("a"), f("b"), f("c"));
        assert_eq!(step(a, a, a), Step::Same);
        assert_eq!(step(a, b, a), Step::Same, "only ours changed");
        assert_eq!(step(a, b, b), Step::Same, "same change on both sides");
        assert_eq!(step(a, a, b), Step::Done(Resolution::Take(b)));
        assert_eq!(step(a, a, None), Step::Done(Resolution::Take(None)), "theirs deleted");
        assert_eq!(step(None, None, b), Step::Done(Resolution::Take(b)), "theirs added");
        assert_eq!(step(a, None, b), Step::Conflict(ConflictKind::DeleteModify));
        assert_eq!(step(a, b, None), Step::Conflict(ConflictKind::ModifyDelete));
        assert_eq!(step(None, b, c), Step::Conflict(ConflictKind::AddAdd));
        assert_eq!(step(a, None, None), Step::Same, "both deleted");
        assert_eq!(step(a, b, c), Step::Lines { mode: Mode::File });
        let link = with_mode(Mode::Symlink, "t");
        assert_eq!(step(a, link, c), Step::Conflict(ConflictKind::Symlink));
        // Mode and content merge separately: theirs made it executable, ours edited it
        let exec_a = with_mode(Mode::Executable, "a");
        let exec_b = with_mode(Mode::Executable, "b");
        assert_eq!(step(a, b, exec_a), Step::Done(Resolution::Take(exec_b)));
        assert_eq!(step(a, exec_a, b), Step::Done(Resolution::Take(exec_b)));
        assert_eq!(step(a, exec_b, c), Step::Lines { mode: Mode::Executable });
    }

    #[test]
    fn line_merges() {
        let base = b"1\n2\n3\n4\n5\n";
        let ours = b"one\n2\n3\n4\n5\n";
        let theirs = b"1\n2\n3\n4\nfive\n";
        assert_eq!(merge_lines(base, ours, theirs).expect("clean"), b"one\n2\n3\n4\nfive\n");
        let (kind, marked) = merge_lines(base, b"x\n2\n3\n4\n5\n", b"y\n2\n3\n4\n5\n").expect_err("overlap");
        assert_eq!(kind, ConflictKind::Content);
        let marked = String::from_utf8(marked.expect("markers")).expect("utf8");
        assert!(marked.contains("<<<<<<<") && marked.contains(">>>>>>>"), "{marked}");
        assert_eq!(
            merge_lines(b"a\0", b"b\0", b"c\0").map_err(|e| e.0),
            Err(ConflictKind::Binary)
        );
        let big = vec![b'a'; MAX_MERGE_BYTES + 1];
        assert_eq!(
            merge_lines(&big, b"x", b"y").map_err(|e| e.0),
            Err(ConflictKind::TooLarge)
        );
    }

    #[test]
    fn markers_and_kind_names() {
        assert!(has_markers(b"a\n<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n"));
        assert!(!has_markers(b"a <<<<<<< not at line start\n>>>>>>> b\n"));
        assert!(!has_markers(b"<<<<<<< only one side\n"));
        let kinds = [
            ConflictKind::Content,
            ConflictKind::AddAdd,
            ConflictKind::ModifyDelete,
            ConflictKind::DeleteModify,
            ConflictKind::Binary,
            ConflictKind::TooLarge,
            ConflictKind::Symlink,
            ConflictKind::Path,
        ];
        let names: std::collections::BTreeSet<_> = kinds.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), kinds.len());
    }

    #[test]
    fn file_directory_clashes() {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
        assert!(path_clashes(&set(&["a/b", "a/c", "ab"])).is_empty());
        assert_eq!(path_clashes(&set(&["a", "a/b/c", "x"])), ["a", "a/b/c"]);
    }
}
