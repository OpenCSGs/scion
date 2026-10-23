//! Reading the pack a push sends (design §5): whole objects and deltas against objects of the
//! same pack (the gateway advertises `no-thin`), each object's SHA computed, the trailer checked.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};

use bytes::Bytes;
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

/// Where a resolved object's content is: in memory, or in the spill file.
#[derive(Debug, Clone, Copy)]
enum Content {
    Mem(usize),
    Spilled { offset: u64, len: usize },
}

/// Contents kept in memory up to a budget, then in a temporary file.
#[derive(Debug, Default)]
struct Store {
    mem: Vec<Bytes>,
    mem_bytes: usize,
    budget: usize,
    spill: Option<std::fs::File>,
    spill_len: u64,
}

impl Store {
    fn put(&mut self, data: Bytes) -> Result<Content> {
        if self.mem_bytes + data.len() <= self.budget {
            self.mem_bytes += data.len();
            self.mem.push(data);
            return Ok(Content::Mem(self.mem.len() - 1));
        }
        let file = match &mut self.spill {
            Some(f) => f,
            None => self
                .spill
                .insert(tempfile::tempfile().map_err(|e| Error::Internal(format!("spill file: {e}")))?),
        };
        file.write_all(&data)
            .map_err(|e| Error::Internal(format!("spill file: {e}")))?;
        let c = Content::Spilled {
            offset: self.spill_len,
            len: data.len(),
        };
        self.spill_len += data.len() as u64;
        Ok(c)
    }

    fn get(&self, c: Content) -> Result<Bytes> {
        match c {
            Content::Mem(i) => Ok(self.mem[i].clone()),
            Content::Spilled { offset, len } => {
                use std::os::unix::fs::FileExt;
                let mut buf = vec![0; len];
                let file = self
                    .spill
                    .as_ref()
                    .ok_or_else(|| Error::Internal("no spill file".into()))?;
                file.read_exact_at(&mut buf, offset)
                    .map_err(|e| Error::Internal(format!("spill file: {e}")))?;
                Ok(Bytes::from(buf))
            }
        }
    }
}

/// The objects of a pack by git SHA, in memory up to a budget and in a temporary file beyond it.
#[derive(Debug, Default)]
pub struct Unpacked {
    objects: HashMap<[u8; 20], (ObjType, Content)>,
    store: Store,
}

impl Unpacked {
    /// An object's type and content.
    pub fn get(&self, sha: &[u8; 20]) -> Result<Option<(ObjType, Bytes)>> {
        match self.objects.get(sha) {
            Some((t, c)) => Ok(Some((*t, self.store.get(*c)?))),
            None => Ok(None),
        }
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn add(&mut self, t: ObjType, content: Bytes) -> Result<[u8; 20]> {
        let sha = object_sha(t, &content);
        if !self.objects.contains_key(&sha) {
            let c = self.store.put(content)?;
            self.objects.insert(sha, (t, c));
        }
        Ok(sha)
    }
}

enum Base {
    Offset(u64),
    Sha([u8; 20]),
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Protocol(format!("bad pack: {}", msg.into()))
}

/// A reader that counts and hashes what is consumed from it.
struct Hashing<R> {
    inner: R,
    hash: Sha1,
    pos: u64,
}

impl<R: BufRead> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let avail = self.fill_buf()?;
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Hashing<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        // The buffer is filled already: this returns it without reading
        if let Ok(b) = self.inner.fill_buf() {
            self.hash.update(&b[..n.min(b.len())]);
        }
        self.pos += n as u64;
        self.inner.consume(n);
    }
}

/// The memory a push's objects may take before they go to a temporary file.
pub const MEMORY_BUDGET: usize = 256 << 20;

/// Parses and resolves a pack read from `input`. `max_object` limits each object's size.
pub fn unpack<R: BufRead>(input: R, max_object: u64) -> Result<Unpacked> {
    unpack_with(input, max_object, MEMORY_BUDGET)
}

pub fn unpack_with<R: BufRead>(input: R, max_object: u64, budget: usize) -> Result<Unpacked> {
    let io = |e: std::io::Error| bad(e.to_string());
    let mut r = Hashing {
        inner: input,
        hash: Sha1::new(),
        pos: 0,
    };
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| bad("no PACK header"))?;
    if &head[..4] != b"PACK" {
        return Err(bad("no PACK header"));
    }
    let version = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
    if version != 2 && version != 3 {
        return Err(bad(format!("version {version}")));
    }
    let count = u32::from_be_bytes([head[8], head[9], head[10], head[11]]);
    let mut out = Unpacked {
        store: Store {
            budget,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut by_offset: HashMap<u64, [u8; 20]> = HashMap::new();
    let mut pending: Vec<(u64, Base, Content)> = Vec::new();
    for _ in 0..count {
        let start = r.pos;
        let (kind, size) = entry_header(&mut r)?;
        if size > max_object {
            return Err(Error::Unsupported(format!(
                "an object of {size} bytes is larger than the {max_object}-byte limit"
            )));
        }
        match kind {
            1..=4 => {
                let t = [ObjType::Commit, ObjType::Tree, ObjType::Blob, ObjType::Tag][kind as usize - 1];
                let content = inflate(&mut r, size)?;
                let sha = out.add(t, content)?;
                by_offset.insert(start, sha);
            }
            6 => {
                let back = ofs(&mut r)?;
                let base = start
                    .checked_sub(back)
                    .ok_or_else(|| bad("delta base before the pack"))?;
                let delta = inflate(&mut r, size)?;
                pending.push((start, Base::Offset(base), out.store.put(delta)?));
            }
            7 => {
                let mut sha = [0u8; 20];
                r.read_exact(&mut sha).map_err(|_| bad("truncated delta base"))?;
                let delta = inflate(&mut r, size)?;
                pending.push((start, Base::Sha(sha), out.store.put(delta)?));
            }
            k => return Err(bad(format!("object type {k}"))),
        }
    }
    let digest = r.hash.clone().finalize();
    let mut trailer = [0u8; 20];
    r.read_exact(&mut trailer).map_err(|_| bad("truncated pack"))?;
    if digest[..] != trailer {
        return Err(bad("checksum mismatch"));
    }
    let mut rest = [0u8; 1];
    if r.read(&mut rest).map_err(io)? != 0 {
        return Err(bad("trailing bytes"));
    }
    // Deltas, bases first: an ofs-delta's base comes earlier in the pack, a ref-delta's base
    // may come anywhere, so passes repeat until nothing is left
    while !pending.is_empty() {
        let before = pending.len();
        let mut left = Vec::new();
        for (off, base, delta) in pending {
            let base_sha = match &base {
                Base::Offset(o) => by_offset.get(o).copied(),
                Base::Sha(s) => out.objects.contains_key(s).then_some(*s),
            };
            let Some((t, base_content)) = (match base_sha {
                Some(s) => out.get(&s)?,
                None => None,
            }) else {
                left.push((off, base, delta));
                continue;
            };
            let d = out.store.get(delta)?;
            let content = Bytes::from(apply_delta(&base_content, &d, max_object)?);
            let sha = out.add(t, content)?;
            by_offset.insert(off, sha);
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

fn byte<R: Read>(r: &mut R, what: &str) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).map_err(|_| bad(format!("truncated {what}")))?;
    Ok(b[0])
}

/// An entry's type and inflated size.
fn entry_header<R: Read>(r: &mut R) -> Result<(u8, u64)> {
    let first = byte(r, "entry")?;
    let kind = (first >> 4) & 7;
    let mut size = u64::from(first & 0x0f);
    let mut shift = 4;
    let mut b = first;
    while b & 0x80 != 0 {
        b = byte(r, "entry size")?;
        if shift > 57 {
            return Err(bad("entry size overflows"));
        }
        size |= u64::from(b & 0x7f) << shift;
        shift += 7;
    }
    Ok((kind, size))
}

/// An ofs-delta's distance back to its base: big-endian 7-bit groups, each continuation adding one.
fn ofs<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = byte(r, "delta offset")?;
    let mut v = u64::from(b & 0x7f);
    while b & 0x80 != 0 {
        b = byte(r, "delta offset")?;
        v = v
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .ok_or_else(|| bad("delta offset overflows"))?
            | u64::from(b & 0x7f);
    }
    Ok(v)
}

/// Inflates one zlib stream of `size` bytes, consuming exactly the stream from `r`.
fn inflate<R: BufRead>(r: &mut R, size: u64) -> Result<Bytes> {
    let mut out = Vec::with_capacity(usize::try_from(size.min(1 << 24)).unwrap_or(0));
    // One spare byte: more output than the header declares is an error
    flate2::bufread::ZlibDecoder::new(r)
        .take(size + 1)
        .read_to_end(&mut out)
        .map_err(|e| bad(format!("zlib: {e}")))?;
    if out.len() as u64 != size {
        return Err(bad(format!(
            "an object's size does not match its header ({size} bytes)"
        )));
    }
    Ok(Bytes::from(out))
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
        let u = unpack(&pack[..], 1 << 20).expect("unpack");
        let blob = scion_query::parse_git_sha("ce013625030ba8dba906f756967f9e9ca394464a").expect("sha");
        assert_eq!(u.get(&blob), Ok(Some((ObjType::Blob, Bytes::from_static(b"hello\n")))));
        assert_eq!(u.len(), 2);
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
            assert!(unpack(&p[..], 1 << 20).is_err(), "{name}");
        }
        assert!(matches!(unpack(&good[..], 0), Err(Error::Unsupported(_))));
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(unpack(&trailing[..], 1 << 20).is_err());
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
        let u = unpack(&pack[..], 1 << 20).expect("unpack");
        assert_eq!(
            u.get(&mid).expect("get").expect("mid").1,
            Bytes::from_static(b"hello gitd")
        );
        assert_eq!(
            u.get(&object_sha(ObjType::Blob, b"hello"))
                .expect("get")
                .expect("hello")
                .1,
            Bytes::from_static(b"hello")
        );
        // A ref-delta on an object the pack lacks: a thin pack
        let (_, head) = PackWriter::new(1);
        let mut thin = head.to_vec();
        thin.extend_from_slice(&raw_entry(7, &d2, None, Some(&[9; 20])));
        let sum = Sha1::digest(&thin);
        thin.extend_from_slice(&sum);
        assert!(matches!(unpack(&thin[..], 1 << 20), Err(Error::Unsupported(m)) if m.contains("thin")));
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
            assert_eq!(ofs(&mut &encode_ofs(v)[..]).expect("ofs"), v as u64, "{v}");
        }
        assert!(ofs(&mut &[0x80][..]).is_err());
        assert!(entry_header(&mut &[0x90][..]).is_err());
    }

    #[test]
    fn spills_past_the_memory_budget() {
        let big = vec![b'x'; 5000];
        let (mut w, head) = PackWriter::new(3);
        let mut pack = head.to_vec();
        for k in [&b"small"[..], &big, b"another"] {
            pack.extend_from_slice(&w.object(Kind::Blob, k));
        }
        pack.extend_from_slice(&w.finish());
        // A budget of 10 bytes: "small" stays in memory, the rest goes to the file
        let u = unpack_with(&pack[..], 1 << 20, 10).expect("unpack");
        for k in [&b"small"[..], &big, b"another"] {
            let got = u.get(&object_sha(ObjType::Blob, k)).expect("get").expect("object");
            assert_eq!(&got.1[..], k);
        }
        assert!(u.store.spill.is_some());
        assert!(!u.is_empty());
        assert_eq!(u.get(&[0; 20]), Ok(None));
    }
}
