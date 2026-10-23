//! git's pkt-line framing: a 4-digit hex length (counting itself) and the payload, or one of the
//! special packets `0000` (flush), `0001` (delim) and `0002` (response end).

use bytes::{BufMut, Bytes, BytesMut};

/// The largest packet git accepts, length prefix included.
pub const MAX_PKT: usize = 65520;
/// The largest payload of a side-band-64k data packet: the packet less the prefix and band byte.
pub const MAX_BAND_DATA: usize = MAX_PKT - 5;

pub const FLUSH: &[u8] = b"0000";
pub const DELIM: &[u8] = b"0001";

/// Appends one data packet.
pub fn put(out: &mut BytesMut, payload: &[u8]) {
    debug_assert!(payload.len() + 4 <= MAX_PKT);
    out.put_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.put_slice(payload);
}

/// Appends a text packet: `line` and a newline.
pub fn put_line(out: &mut BytesMut, line: &str) {
    out.put_slice(format!("{:04x}", line.len() + 5).as_bytes());
    out.put_slice(line.as_bytes());
    out.put_u8(b'\n');
}

/// One packet of `data` on side band `band` (1 data, 2 progress, 3 error).
pub fn band(band: u8, data: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(data.len() + 5);
    out.put_slice(format!("{:04x}", data.len() + 5).as_bytes());
    out.put_u8(band);
    out.put_slice(data);
    out.freeze()
}

/// A packet read from a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkt<'a> {
    Flush,
    Delim,
    End,
    /// The payload, without a trailing newline.
    Data(&'a [u8]),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed pkt-line: {0}")]
pub struct PktError(pub String);

/// Reads packets from a whole request body.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// What follows the packets read so far (a push's pack).
    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }

    /// The next packet, or `None` at the end of the body.
    pub fn read(&mut self) -> Result<Option<Pkt<'a>>, PktError> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let Some(head) = self.buf.get(..4) else {
            return Err(PktError("truncated length".into()));
        };
        let len = std::str::from_utf8(head)
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| PktError(format!("bad length {:?}", String::from_utf8_lossy(head))))?;
        let pkt = match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::End,
            3 => return Err(PktError("length 3".into())),
            _ => {
                let Some(body) = self.buf.get(4..len) else {
                    return Err(PktError("truncated packet".into()));
                };
                Pkt::Data(body.strip_suffix(b"\n").unwrap_or(body))
            }
        };
        self.buf = &self.buf[len.max(4)..];
        Ok(Some(pkt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_packets() {
        let mut out = BytesMut::new();
        put_line(&mut out, "# service=git-upload-pack");
        out.put_slice(FLUSH);
        put(&mut out, b"a\0b");
        assert_eq!(&out[..], b"001e# service=git-upload-pack\n00000007a\0b");
        assert_eq!(&band(2, b"hi")[..], b"0007\x02hi");
    }

    #[test]
    fn reads_packets() {
        let body = b"0032want 0123456789012345678901234567890123456789\n00000001000bhave x\n0008done0002";
        let mut r = Reader::new(body);
        assert_eq!(
            r.read(),
            Ok(Some(Pkt::Data(b"want 0123456789012345678901234567890123456789")))
        );
        assert_eq!(r.read(), Ok(Some(Pkt::Flush)));
        assert_eq!(r.read(), Ok(Some(Pkt::Delim)));
        assert_eq!(r.read(), Ok(Some(Pkt::Data(b"have x"))));
        assert_eq!(r.read(), Ok(Some(Pkt::Data(b"done"))));
        assert_eq!(r.read(), Ok(Some(Pkt::End)));
        assert_eq!(r.read(), Ok(None));
    }

    #[test]
    fn rejects_malformed_packets() {
        for bad in [&b"00"[..], b"zzzz", b"0003", b"0010abc"] {
            assert!(Reader::new(bad).read().is_err(), "{bad:?}");
        }
    }
}
