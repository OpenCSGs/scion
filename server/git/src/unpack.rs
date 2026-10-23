//! Reading the pack a push sends (design §5): whole objects and deltas against objects of the
//! same pack (the gateway advertises `no-thin`), each object's SHA computed, the trailer checked.

use std::collections::HashMap;

use bytes::Bytes;
use flate2::{Decompress, FlushDecompress, Status};
use sha1::{Digest, Sha1};

use crate::{Error, Result};

/// git's object types in a pack, deltas resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjType {
    fn name(self) -> &'static str {
        match self {
            ObjType::Commit => "commit",
            ObjType::Tree => "tree",
            ObjType::Blob => "blob",
            ObjType::Tag => "tag",
        }
    }
}

/// The objects of a pack by git SHA.
#[derive(Debug, Default)]
pub struct Unpacked {
    pub objects: HashMap<[u8; 20], (ObjType, Bytes)>,
}

enum Entry {
    Whole(ObjType, Bytes),
    OfsDelta(usize, Bytes),
    RefDelta([u8; 20], Bytes),
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Protocol(format!("bad pack: {}", msg.into()))
}

/// Parses and resolves a whole pack. `max_object` limits each object's size.
pub fn unpack(data: &[u8], max_object: u64) -> Result<Unpacked> {
    if data.len() < 32 || &data[..4] != b"PACK" {
        return Err(bad("no PACK header"));
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if version != 2 && version != 3 {
        return Err(bad(format!("version {version}")));
    }
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let body_end = data.len() - 20;
    if Sha1::digest(&data[..body_end])[..] != data[body_end..] {
        return Err(bad("checksum mismatch"));
    }
    let mut entries: Vec<(usize, Entry)> = Vec::with_capacity(count.min(1 << 20));
    let mut pos = 12;
    for _ in 0..count {
        let start = pos;
        let (kind, size, used) = entry_header(&data[pos..body_end])?;
        pos += used;
        if size > max_object {
            return Err(Error::Unsupported(format!(
                "an object of {size} bytes is larger than the {max_object}-byte limit"
            )));
        }
        let entry = match kind {
            1..=4 => {
                let (content, used) = inflate(&data[pos..body_end], size)?;
                pos += used;
                let t = [ObjType::Commit, ObjType::Tree, ObjType::Blob, ObjType::Tag][kind as usize - 1];
                Entry::Whole(t, content)
            }
            6 => {
                let (back, used) = ofs(&data[pos..body_end])?;
                pos += used;
                let base = start
                    .checked_sub(back)
                    .ok_or_else(|| bad("delta base before the pack"))?;
                let (delta, used) = inflate(&data[pos..body_end], size)?;
                pos += used;
                Entry::OfsDelta(base, delta)
            }
            7 => {
                let sha: [u8; 20] = data
                    .get(pos..pos + 20)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| bad("truncated delta base"))?;
                pos += 20;
                let (delta, used) = inflate(&data[pos..body_end], size)?;
                pos += used;
                Entry::RefDelta(sha, delta)
            }
            k => return Err(bad(format!("object type {k}"))),
        };
        entries.push((start, entry));
    }
    if pos != body_end {
        return Err(bad("trailing bytes"));
    }
    resolve(entries, max_object)
}

/// Resolves deltas, bases first: an ofs-delta's base comes earlier in the pack, a ref-delta's
/// base may come anywhere, so passes repeat until nothing is left.
fn resolve(entries: Vec<(usize, Entry)>, max_object: u64) -> Result<Unpacked> {
    let mut by_offset: HashMap<usize, (ObjType, Bytes)> = HashMap::new();
    let mut out = Unpacked::default();
    let mut pending: Vec<(usize, Entry)> = Vec::new();
    for (off, e) in entries {
        match e {
            Entry::Whole(t, content) => {
                out.objects.insert(object_sha(t, &content), (t, content.clone()));
                by_offset.insert(off, (t, content));
            }
            other => pending.push((off, other)),
        }
    }
    while !pending.is_empty() {
        let before = pending.len();
        let mut left = Vec::new();
        for (off, e) in pending {
            let base = match &e {
                Entry::OfsDelta(base, _) => by_offset.get(base).cloned(),
                Entry::RefDelta(sha, _) => out.objects.get(sha).cloned(),
                Entry::Whole(..) => unreachable!("whole objects are resolved first"),
            };
            let Some((t, base)) = base else {
                left.push((off, e));
                continue;
            };
            let (Entry::OfsDelta(_, delta) | Entry::RefDelta(_, delta)) = &e else {
                unreachable!("only deltas are pending")
            };
            let content = Bytes::from(apply_delta(&base, delta, max_object)?);
            out.objects.insert(object_sha(t, &content), (t, content.clone()));
            by_offset.insert(off, (t, content));
        }
        if left.len() == before {
            return Err(Error::Unsupported(
                "the pack has deltas against objects it does not contain (a thin pack)".into(),
            ));
        }
        pending = left;
    }
    Ok(out)
}

pub fn object_sha(t: ObjType, content: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", t.name(), content.len()).as_bytes());
    h.update(content);
    h.finalize().into()
}

/// An entry's type, inflated size and header length.
fn entry_header(b: &[u8]) -> Result<(u8, u64, usize)> {
    let first = *b.first().ok_or_else(|| bad("truncated entry"))?;
    let kind = (first >> 4) & 7;
    let mut size = u64::from(first & 0x0f);
    let mut shift = 4;
    let mut i = 1;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = *b.get(i).ok_or_else(|| bad("truncated entry size"))?;
        if shift > 57 {
            return Err(bad("entry size overflows"));
        }
        size |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        i += 1;
    }
    Ok((kind, size, i))
}

/// An ofs-delta's distance back to its base: big-endian 7-bit groups, each continuation adding one.
fn ofs(b: &[u8]) -> Result<(usize, usize)> {
    let mut i = 0;
    let mut byte = *b.first().ok_or_else(|| bad("truncated delta offset"))?;
    let mut v = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        i += 1;
        byte = *b.get(i).ok_or_else(|| bad("truncated delta offset"))?;
        v = v
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .ok_or_else(|| bad("delta offset overflows"))?
            | usize::from(byte & 0x7f);
    }
    Ok((v, i + 1))
}

/// Inflates one zlib stream of `size` bytes; returns it and how many input bytes it took.
fn inflate(input: &[u8], size: u64) -> Result<(Bytes, usize)> {
    let size = usize::try_from(size).map_err(|_| bad("object too large"))?;
    // One spare byte: more output than the header declares fills it, and is an error
    let mut out = Vec::with_capacity(size + 1);
    let mut z = Decompress::new(true);
    loop {
        let (inp, outp) = (z.total_in(), z.total_out());
        let status = z
            .decompress_vec(&input[inp as usize..], &mut out, FlushDecompress::None)
            .map_err(|e| bad(format!("zlib: {e}")))?;
        if status == Status::StreamEnd {
            break;
        }
        if out.len() > size {
            break;
        }
        if z.total_in() == inp && z.total_out() == outp {
            return Err(bad("truncated zlib stream"));
        }
    }
    if out.len() != size {
        return Err(bad(format!(
            "an object's size does not match its header ({size} bytes)"
        )));
    }
    Ok((Bytes::from(out), z.total_in() as usize))
}

fn varint(d: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *d.get(*pos).ok_or_else(|| bad("truncated delta"))?;
        *pos += 1;
        if shift > 63 {
            return Err(bad("delta size overflows"));
        }
        v |= u64::from(b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
}

/// git's delta format: the base and result sizes, then copies from the base and inserts.
pub fn apply_delta(base: &[u8], d: &[u8], max_object: u64) -> Result<Vec<u8>> {
    let mut pos = 0;
    let src = varint(d, &mut pos)?;
    let dst = varint(d, &mut pos)?;
    if src != base.len() as u64 {
        return Err(bad("delta base size mismatch"));
    }
    if dst > max_object {
        return Err(Error::Unsupported(format!(
            "an object of {dst} bytes is larger than the {max_object}-byte limit"
        )));
    }
    let mut out = Vec::with_capacity(dst as usize);
    while pos < d.len() {
        let op = d[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut field = |bits: std::ops::Range<u8>| -> Result<usize> {
                let mut v = 0usize;
                for (i, bit) in bits.enumerate() {
                    if op & (1 << bit) != 0 {
                        let b = *d.get(pos).ok_or_else(|| bad("truncated delta copy"))?;
                        pos += 1;
                        v |= usize::from(b) << (8 * i);
                    }
                }
                Ok(v)
            };
            let off = field(0..4)?;
            let mut len = field(4..7)?;
            if len == 0 {
                len = 0x10000;
            }
            let chunk = off
                .checked_add(len)
                .and_then(|end| base.get(off..end))
                .ok_or_else(|| bad("delta copies past its base"))?;
            out.extend_from_slice(chunk);
        } else if op != 0 {
            let n = usize::from(op);
            let chunk = d.get(pos..pos + n).ok_or_else(|| bad("truncated delta insert"))?;
            out.extend_from_slice(chunk);
            pos += n;
        } else {
            return Err(bad("delta opcode 0"));
        }
    }
    if out.len() as u64 != dst {
        return Err(bad("delta result size mismatch"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{Kind, PackWriter};

    fn pack_of(objects: &[(Kind, &[u8])]) -> Vec<u8> {
        let (mut w, head) = PackWriter::new(objects.len() as u32);
        let mut out = head.to_vec();
        for (k, d) in objects {
            out.extend_from_slice(&w.object(*k, d));
        }
        out.extend_from_slice(&w.finish());
        out
    }

    #[test]
    fn reads_whole_objects() {
        let pack = pack_of(&[(Kind::Blob, b"hello\n"), (Kind::Tree, b"")]);
        let u = unpack(&pack, 1 << 20).expect("unpack");
        let blob = scion_query::parse_git_sha("ce013625030ba8dba906f756967f9e9ca394464a").expect("sha");
        assert_eq!(u.objects[&blob], (ObjType::Blob, Bytes::from_static(b"hello\n")));
        assert_eq!(u.objects.len(), 2);
    }

    #[test]
    fn rejects_bad_packs() {
        let good = pack_of(&[(Kind::Blob, b"x")]);
        let mut corrupt = good.clone();
        corrupt[13] ^= 1;
        let mut bad_version = good.clone();
        bad_version[7] = 9;
        for (name, p) in [
            ("short", b"PACK".to_vec()),
            ("magic", b"KCAP".repeat(10)),
            ("checksum", corrupt),
            ("version", bad_version),
        ] {
            assert!(unpack(&p, 1 << 20).is_err(), "{name}");
        }
        assert!(matches!(unpack(&good, 0), Err(Error::Unsupported(_))));
    }

    fn delta(base_len: usize, dst: &[u8], ops: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        for mut v in [base_len, dst.len()] {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                d.push(if v > 0 { b | 0x80 } else { b });
                if v == 0 {
                    break;
                }
            }
        }
        d.extend_from_slice(ops);
        d
    }

    #[test]
    fn applies_deltas() {
        let base = b"hello world";
        // copy 6 bytes from 0, insert "git", copy "d" (offset 10, 1 byte)
        let d = delta(11, b"hello gitd", &[0x90, 6, 3, b'g', b'i', b't', 0x91, 10, 1]);
        assert_eq!(apply_delta(base, &d, 100).expect("apply"), b"hello gitd");
        // A copy past the base, a size mismatch, opcode 0, a result over the limit
        assert!(apply_delta(base, &delta(11, b"x", &[0x91, 10, 5]), 100).is_err());
        assert!(apply_delta(base, &delta(3, b"x", &[1, b'x']), 100).is_err());
        assert!(apply_delta(base, &delta(11, b"x", &[0]), 100).is_err());
        assert!(apply_delta(base, &delta(11, b"xy", &[1, b'x']), 100).is_err());
        assert!(matches!(
            apply_delta(base, &delta(11, b"xy", &[2, b'x', b'y']), 1),
            Err(Error::Unsupported(_))
        ));
        // A copy of size 0 means 0x10000 bytes
        let big = vec![7u8; 0x10000];
        let d = delta(0x10000, &big, &[0x80]);
        assert_eq!(apply_delta(&big, &d, 1 << 20).expect("apply"), big);
    }

    #[test]
    fn resolves_ofs_and_ref_deltas() {
        // A pack with a blob, an ofs-delta on it, and a ref-delta on the delta's result
        let base = b"hello world";
        let (mut w, head) = PackWriter::new(3);
        let mut pack = head.to_vec();
        let first = pack.len();
        pack.extend_from_slice(&w.object(Kind::Blob, base));
        let d1 = delta(11, b"hello gitd", &[0x90, 6, 3, b'g', b'i', b't', 0x91, 10, 1]);
        let second = pack.len();
        pack.extend_from_slice(&raw_entry(6, &d1, Some(&encode_ofs(second - first)), None));
        let mid = object_sha(ObjType::Blob, b"hello gitd");
        let d2 = delta(10, b"hello", &[0x90, 5]);
        pack.extend_from_slice(&raw_entry(7, &d2, None, Some(&mid)));
        let sum = Sha1::digest(&pack);
        pack.extend_from_slice(&sum);
        let u = unpack(&pack, 1 << 20).expect("unpack");
        assert_eq!(u.objects[&mid].1, Bytes::from_static(b"hello gitd"));
        assert_eq!(
            u.objects[&object_sha(ObjType::Blob, b"hello")].1,
            Bytes::from_static(b"hello")
        );
        // A ref-delta on an object the pack lacks: a thin pack
        let (_, head) = PackWriter::new(1);
        let mut thin = head.to_vec();
        thin.extend_from_slice(&raw_entry(7, &d2, None, Some(&[9; 20])));
        let sum = Sha1::digest(&thin);
        thin.extend_from_slice(&sum);
        assert!(matches!(unpack(&thin, 1 << 20), Err(Error::Unsupported(m)) if m.contains("thin")));
    }

    fn encode_ofs(mut v: usize) -> Vec<u8> {
        let mut out = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            v -= 1;
            out.insert(0, 0x80 | (v & 0x7f) as u8);
            v >>= 7;
        }
        out
    }

    fn raw_entry(kind: u8, data: &[u8], ofs: Option<&[u8]>, base: Option<&[u8; 20]>) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let mut size = data.len();
        let mut byte = (kind << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(byte);
        if let Some(o) = ofs {
            out.extend_from_slice(o);
        }
        if let Some(b) = base {
            out.extend_from_slice(b);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(data).expect("zlib");
        out.extend_from_slice(&z.finish().expect("zlib"));
        out
    }

    #[test]
    fn decodes_offsets() {
        for v in [0usize, 1, 127, 128, 300, 16511, 1 << 20] {
            assert_eq!(ofs(&encode_ofs(v)).expect("ofs").0, v, "{v}");
        }
        assert!(ofs(&[0x80]).is_err());
        assert!(entry_header(&[0x90]).is_err());
    }
}
