//! Writing git packfiles (version 2): a header, whole zlib-compressed objects and the SHA-1 of
//! everything before the trailer.

use std::io::Write;

use bytes::{BufMut, Bytes, BytesMut};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use sha1::{Digest, Sha1};

/// git's object types, as numbered in pack entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
        }
    }
}

/// Writes a pack piece by piece, hashing as it goes.
pub struct PackWriter {
    hash: Sha1,
    level: Compression,
}

impl PackWriter {
    /// Starts a pack of `count` objects and returns its header.
    pub fn new(count: u32) -> (Self, Bytes) {
        let mut head = BytesMut::with_capacity(12);
        head.put_slice(b"PACK");
        head.put_u32(2);
        head.put_u32(count);
        let mut hash = Sha1::new();
        hash.update(&head);
        // Fast compression: a clone spends its time reading objects, and level 1 already
        // shrinks source text to about a third.
        let w = Self {
            hash,
            level: Compression::fast(),
        };
        (w, head.freeze())
    }

    /// One whole object: its type and size header, then the compressed content.
    pub fn object(&mut self, kind: Kind, data: &[u8]) -> Bytes {
        let mut out = BytesMut::with_capacity(data.len() / 2 + 16);
        let mut size = data.len() as u64;
        let mut byte = ((kind as u8) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.put_u8(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.put_u8(byte);
        let mut z = ZlibEncoder::new((&mut out).writer(), self.level);
        // Writing to memory cannot fail.
        z.write_all(data).expect("zlib into memory");
        z.finish().expect("zlib into memory");
        self.hash.update(&out);
        out.freeze()
    }

    /// The trailer: the SHA-1 of the pack so far.
    pub fn finish(self) -> Bytes {
        Bytes::copy_from_slice(&self.hash.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    #[test]
    fn writes_a_pack() {
        let (mut w, head) = PackWriter::new(2);
        assert_eq!(&head[..], b"PACK\0\0\0\x02\0\0\0\x02");
        let small = w.object(Kind::Blob, b"hello\n");
        // type 3, size 6: one header byte
        assert_eq!(small[0], 0x36);
        let mut out = Vec::new();
        ZlibDecoder::new(&small[1..]).read_to_end(&mut out).expect("zlib");
        assert_eq!(out, b"hello\n");
        // 300 bytes: 0x2c with the continuation bit, then 300 >> 4 = 18
        let big = w.object(Kind::Tree, &[b'x'; 300]);
        assert_eq!(&big[..2], &[0x80 | 0x20 | 0x0c, 18]);
        let trailer = w.finish();
        let mut all = Sha1::new();
        all.update(&head);
        all.update(&small);
        all.update(&big);
        assert_eq!(&trailer[..], &all.finalize()[..]);
        assert_eq!(Kind::Commit.name(), "commit");
    }
}
