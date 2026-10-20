//! Deltas: an object stored as the difference from another object (its base), as zstd with the
//! base's bytes as the dictionary. Successive versions of a file or directory share most of their
//! bytes, so a delta is typically a few percent of the object.
//!
//! ```text
//! "SCDP" offset len zstd-frame  base in the same pack: where it is (uvarints)
//! "SCDL" base-id zstd-frame     base anywhere else: its full id
//! ```
//!
//! Most deltas are small (a directory with one entry changed takes about 100 bytes), so naming the
//! base by where it is rather than by its 34-byte id saves a fifth of them. It also lets a reader
//! fetch the base without looking it up.
//!
//! Only packs hold deltas, and only [`crate::ObjectBlobStore::repack`] writes them: it knows which
//! object is the next version of which. Readers recognise the magic, read the base (which may
//! itself be a delta) and decompress. The result is checked against the object id like any other
//! read, so a wrong or damaged base shows up as a failed integrity check.

use scion_object::{ID_LEN, ObjectId};

const BY_ID: &[u8; 4] = b"SCDL";
const IN_PACK: &[u8; 4] = b"SCDP";
/// zstd level for deltas: higher than for whole objects, since repacking is a background job.
const LEVEL: i32 = 9;

/// Where a delta's base is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base {
    Id(ObjectId),
    /// The stored bytes at `offset..offset + len` of the same pack.
    At {
        offset: u64,
        len: u64,
    },
}

/// `data` as a delta against `base_bytes` (the object `base`), if that is smaller than `whole`
/// bytes (what storing it on its own would take) by at least 10%.
pub(crate) fn encode(base: Base, base_bytes: &[u8], data: &[u8], whole: usize) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut out = header(base);
    // The base as a ref prefix (what `zstd --patch-from` does), with a window over all of it
    let mut e = zstd::stream::write::Encoder::with_ref_prefix(&mut out, LEVEL, base_bytes).ok()?;
    e.window_log(window(base_bytes.len() + data.len())).ok()?;
    if base_bytes.len() + data.len() > 1 << 20 {
        e.long_distance_matching(true).ok()?;
    }
    e.write_all(data).ok()?;
    e.finish().ok()?;
    (out.len() < whole - whole / 10).then_some(out)
}

fn header(base: Base) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    match base {
        Base::Id(id) => {
            out.extend_from_slice(BY_ID);
            out.extend_from_slice(&id.to_bytes());
        }
        Base::At { offset, len } => {
            out.extend_from_slice(IN_PACK);
            put_uvarint(&mut out, offset);
            put_uvarint(&mut out, len);
        }
    }
    out
}

/// The same delta with its base named another way (a pack rewritten without some objects).
pub(crate) fn rebase(stored: &[u8], base: Base) -> Option<Vec<u8>> {
    let (_, at) = parse(stored)?;
    let mut out = header(base);
    out.extend_from_slice(&stored[at..]);
    Some(out)
}

/// A window (log2) covering `span` bytes.
fn window(span: usize) -> u32 {
    (usize::BITS - span.max(1).leading_zeros()).clamp(10, MAX_WINDOW_LOG)
}

/// Largest window a delta uses: deltas are only made for objects of a few MiB.
const MAX_WINDOW_LOG: u32 = 27;

/// The base of a delta and where its zstd frame starts. None when `stored` is not a delta.
fn parse(stored: &[u8]) -> Option<(Base, usize)> {
    let magic = stored.get(..4)?;
    if magic == BY_ID {
        let id = ObjectId::from_bytes(stored.get(4..4 + ID_LEN)?).ok()?;
        return Some((Base::Id(id), 4 + ID_LEN));
    }
    if magic != IN_PACK {
        return None;
    }
    let (offset, at) = uvarint(stored, 4)?;
    let (len, at) = uvarint(stored, at)?;
    Some((Base::At { offset, len }, at))
}

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A uvarint at `at` (at most 9 bytes, so up to 2^63) and the position after it.
fn uvarint(b: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (n, x) in b.iter().skip(at).take(9).enumerate() {
        v |= u64::from(x & 0x7f) << (7 * n);
        if x & 0x80 == 0 {
            return Some((v, at + n + 1));
        }
    }
    None
}

/// The base a stored object is a delta against; None when it is stored whole.
pub(crate) fn base_of(stored: &[u8]) -> Option<Base> {
    parse(stored).map(|(b, _)| b)
}

/// The object from its delta and its base's bytes. None when the delta does not decode.
pub(crate) fn decode(stored: &[u8], base: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let (_, at) = parse(stored)?;
    let mut d = zstd::stream::read::Decoder::with_ref_prefix(&stored[at..], base).ok()?;
    d.window_log_max(MAX_WINDOW_LOG).ok()?;
    let mut out = Vec::new();
    d.read_to_end(&mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(n: usize) -> Vec<u8> {
        (0..2_000)
            .map(|i| format!("line {i} of version {}\n", if i == n { n } else { 0 }))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn roundtrip_and_size() {
        let (old, new) = (version(1), version(2));
        let base_id = ObjectId::of_blob(&new);
        let whole = zstd::bulk::compress(&old, 3).expect("compress").len();
        let d = encode(Base::Id(base_id), &new, &old, whole).expect("smaller as a delta");
        assert!(d.len() * 10 < whole, "{} vs {whole}", d.len());
        assert_eq!(base_of(&d), Some(Base::Id(base_id)));
        assert_eq!(decode(&d, &new), Some(old.clone()));
        // The wrong base does not give the object back
        assert_ne!(decode(&d, &version(3)), Some(old.clone()));
        // Named by where it is: the same frame, a shorter header
        for (offset, len) in [(0, 0), (127, 128), (70_000, 1 << 20), (u64::MAX >> 1, 5)] {
            let at = Base::At { offset, len };
            let p = rebase(&d, at).expect("rebase");
            assert_eq!(base_of(&p), Some(at));
            assert_eq!(decode(&p, &new), Some(old.clone()));
            assert_eq!(rebase(&p, Base::Id(base_id)), Some(d.clone()));
        }
        let at = Base::At { offset: 5, len: 300 };
        assert_eq!(rebase(&d, at).expect("rebase").len(), d.len() - ID_LEN + 3);
        let small = encode(at, &new, &old, whole).expect("in pack");
        assert_eq!(base_of(&small), Some(at));
    }

    #[test]
    fn not_worth_it_or_not_a_delta() {
        let a = b"short".to_vec();
        let b = b"other".to_vec();
        assert_eq!(encode(Base::Id(ObjectId::of_blob(&b)), &b, &a, a.len()), None);
        assert_eq!(base_of(b"plain bytes"), None);
        assert_eq!(base_of(b"SCDL"), None, "too short");
        assert_eq!(base_of(b"SCDP"), None, "no offset");
        assert_eq!(base_of(b"SCDP\x05"), None, "no length");
        assert_eq!(
            base_of(b"SCDP\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01\x01"),
            None,
            "offset too long"
        );
        assert_eq!(rebase(b"plain", Base::At { offset: 1, len: 1 }), None);
        let mut bad = BY_ID.to_vec();
        bad.extend_from_slice(&[0xff; ID_LEN]);
        assert_eq!(base_of(&bad), None, "corrupt base id");
        assert_eq!(decode(b"SC", b""), None);
        let mut junk = BY_ID.to_vec();
        junk.extend_from_slice(&ObjectId::of_blob(b"x").to_bytes());
        junk.extend_from_slice(b"not zstd");
        assert_eq!(decode(&junk, b"x"), None);
    }

    #[test]
    fn large_objects_reach_across_the_whole_base() {
        // 6 MiB of noise with a small change: only a window covering the base finds the match
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let new: Vec<u8> = (0..6 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let mut old = new.clone();
        old[100] ^= 1;
        let d = encode(Base::Id(ObjectId::of_blob(&new)), &new, &old, old.len()).expect("delta");
        assert!(d.len() < 4096, "{}", d.len());
        assert_eq!(decode(&d, &new), Some(old));
    }
}
