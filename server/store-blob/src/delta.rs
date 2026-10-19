//! Deltas: an object stored as the difference from another object (its base), as zstd with the
//! base's bytes as the dictionary. Successive versions of a file or directory share most of their
//! bytes, so a delta is typically a few percent of the object.
//!
//! ```text
//! "SCDL" base-id zstd-frame
//! ```
//!
//! Only packs hold deltas, and only [`crate::ObjectBlobStore::repack`] writes them: it knows which
//! object is the next version of which. Readers recognise the magic, read the base (which may
//! itself be a delta) and decompress. The result is checked against the object id like any other
//! read, so a wrong or damaged base shows up as a failed integrity check.

use scion_object::{ID_LEN, ObjectId};

const MAGIC: &[u8; 4] = b"SCDL";
const HEADER: usize = MAGIC.len() + ID_LEN;
/// zstd level for deltas: higher than for whole objects, since repacking is a background job.
const LEVEL: i32 = 9;

/// `data` as a delta against `base` (whose id is `base_id`), if that is smaller than `whole`
/// bytes (what storing it on its own would take) by at least 10%.
pub(crate) fn encode(base_id: &ObjectId, base: &[u8], data: &[u8], whole: usize) -> Option<Vec<u8>> {
    use std::io::Write;
    // The base as a ref prefix (what `zstd --patch-from` does), with a window over all of it
    let mut out = Vec::with_capacity(HEADER + data.len() / 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&base_id.to_bytes());
    let mut e = zstd::stream::write::Encoder::with_ref_prefix(&mut out, LEVEL, base).ok()?;
    e.window_log(window(base.len() + data.len())).ok()?;
    if base.len() + data.len() > 1 << 20 {
        e.long_distance_matching(true).ok()?;
    }
    e.write_all(data).ok()?;
    e.finish().ok()?;
    (out.len() < whole - whole / 10).then_some(out)
}

/// A window (log2) covering `span` bytes.
fn window(span: usize) -> u32 {
    (usize::BITS - span.max(1).leading_zeros()).clamp(10, MAX_WINDOW_LOG)
}

/// Largest window a delta uses: deltas are only made for objects of a few MiB.
const MAX_WINDOW_LOG: u32 = 27;

/// The base a stored object is a delta against; None when it is stored whole.
pub(crate) fn base_of(stored: &[u8]) -> Option<ObjectId> {
    if stored.len() < HEADER || !stored.starts_with(MAGIC) {
        return None;
    }
    ObjectId::from_bytes(&stored[MAGIC.len()..HEADER]).ok()
}

/// The object from its delta and its base's bytes. None when the delta does not decode.
pub(crate) fn decode(stored: &[u8], base: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut d = zstd::stream::read::Decoder::with_ref_prefix(stored.get(HEADER..)?, base).ok()?;
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
        let d = encode(&base_id, &new, &old, whole).expect("smaller as a delta");
        assert!(d.len() * 10 < whole, "{} vs {whole}", d.len());
        assert_eq!(base_of(&d), Some(base_id));
        assert_eq!(decode(&d, &new), Some(old.clone()));
        // The wrong base does not give the object back
        assert_ne!(decode(&d, &version(3)), Some(old));
    }

    #[test]
    fn not_worth_it_or_not_a_delta() {
        let a = b"short".to_vec();
        let b = b"other".to_vec();
        assert_eq!(encode(&ObjectId::of_blob(&b), &b, &a, a.len()), None);
        assert_eq!(base_of(b"plain bytes"), None);
        assert_eq!(base_of(b"SCDL"), None, "too short");
        let mut bad = MAGIC.to_vec();
        bad.extend_from_slice(&[0xff; ID_LEN]);
        assert_eq!(base_of(&bad), None, "corrupt base id");
        assert_eq!(decode(b"SC", b""), None);
        let mut junk = MAGIC.to_vec();
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
        let d = encode(&ObjectId::of_blob(&new), &new, &old, old.len()).expect("delta");
        assert!(d.len() < 4096, "{}", d.len());
        assert_eq!(decode(&d, &new), Some(old));
    }
}
