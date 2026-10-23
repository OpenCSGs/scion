//! git's commit object format, shared by the server (which computes git SHAs) and the importer
//! (which checks whether a git commit can be rebuilt from Scion's fields).

use crate::{ObjectId, Signature};

/// The header that names a commit's Scion id in its computed git object.
pub const SCION_ID_HEADER: &str = "scion-id";

/// `name <email> seconds ±hhmm`. git allows no `<`, `>` or newline in names and emails, so they
/// are dropped.
pub fn ident(s: &Signature) -> String {
    let clean = |v: &str| {
        v.chars()
            .filter(|c| !matches!(c, '<' | '>' | '\n'))
            .collect::<String>()
            .trim()
            .to_string()
    };
    let tz = s.tz_offset_minutes;
    let sign = if tz < 0 { '-' } else { '+' };
    let abs = tz.unsigned_abs();
    format!(
        "{} <{}> {} {sign}{:02}{:02}",
        clean(&s.name),
        clean(&s.email),
        s.time_ms.div_euclid(1000),
        abs / 60,
        abs % 60
    )
}

/// A git commit object built from these fields, with a `scion-id` header when `id` is given.
pub fn commit_body(
    id: Option<ObjectId>,
    tree: &[u8; 20],
    parents: &[[u8; 20]],
    author: &Signature,
    committer: &Signature,
    message: &str,
) -> Vec<u8> {
    let mut out = format!("tree {}\n", hex::encode(tree));
    for p in parents {
        out.push_str(&format!("parent {}\n", hex::encode(p)));
    }
    out.push_str(&format!("author {}\n", ident(author)));
    out.push_str(&format!("committer {}\n", ident(committer)));
    if let Some(id) = id {
        out.push_str(&format!("{SCION_ID_HEADER} {id}\n"));
    }
    out.push('\n');
    out.push_str(message);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, time_ms: i64, tz: i32) -> Signature {
        Signature {
            name: name.into(),
            email: "e@x".into(),
            time_ms,
            tz_offset_minutes: tz,
        }
    }

    #[test]
    fn formats_identities_and_commits() {
        assert_eq!(
            ident(&sig(" A <B>\n", 1_700_000_000_999, 480)),
            "A B <e@x> 1700000000 +0800"
        );
        // Before 1970, seconds round down
        assert_eq!(ident(&sig("B", -1, -90)), "B <e@x> -1 -0130");
        let body = commit_body(
            None,
            &[0xaa; 20],
            &[[0x11; 20]],
            &sig("A", 0, 0),
            &sig("C", 0, 0),
            "m\n",
        );
        let want = format!(
            "tree {}\nparent {}\nauthor A <e@x> 0 +0000\ncommitter C <e@x> 0 +0000\n\nm\n",
            "aa".repeat(20),
            "11".repeat(20)
        );
        assert_eq!(String::from_utf8(body).expect("utf8"), want);
        let id = ObjectId::of_blob(b"x");
        let body = commit_body(Some(id), &[0; 20], &[], &sig("A", 0, 0), &sig("A", 0, 0), "");
        assert!(
            String::from_utf8(body)
                .expect("utf8")
                .contains(&format!("\nscion-id {id}\n\n"))
        );
    }
}
