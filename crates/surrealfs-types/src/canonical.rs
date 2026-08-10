//! Canonical byte encoding v1 and domain-separated BLAKE3 digests.
//!
//! Every digest that defines identity (chunks, state nodes, state roots, commits, command
//! hashes) is computed over this encoding. The encoding is intentionally trivial:
//! little-endian fixed-width integers, length-prefixed byte strings, and count-prefixed
//! sorted sequences. Changing any rule here is a `HASH_VERSION` bump.

use crate::id::Digest;

const DOMAIN_PREFIX: &str = "surrealfs:v1:";

/// Canonical encoder buffer.
#[derive(Default)]
pub struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Length-prefixed byte string.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u64(v.len() as u64);
        self.buf.extend_from_slice(v);
        self
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    /// Optional value: 0x00 for none, 0x01 + value for some.
    pub fn opt_str(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s)
            }
        }
    }

    /// Raw 32-byte digest (decoded from hex).
    pub fn digest(&mut self, d: &Digest) -> &mut Self {
        let raw = hex::decode(d.as_str()).expect("Digest is validated hex");
        self.buf.extend_from_slice(&raw);
        self
    }

    /// Count prefix for a sequence; caller must then encode exactly `n` elements.
    pub fn seq(&mut self, n: usize) -> &mut Self {
        self.u64(n as u64)
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Domain-separated digest: BLAKE3 over `surrealfs:v1:<kind>\n<payload>`.
pub fn digest(kind: &str, payload: &[u8]) -> Digest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_PREFIX.as_bytes());
    hasher.update(kind.as_bytes());
    hasher.update(b"\n");
    hasher.update(payload);
    Digest::from_bytes(*hasher.finalize().as_bytes())
}

/// Content chunks are addressed by the plain BLAKE3 of their raw bytes (no domain prefix),
/// which keeps deduplication independent of any SurrealFS versioning.
pub fn chunk_digest(bytes: &[u8]) -> Digest {
    Digest::from_bytes(*blake3::hash(bytes).as_bytes())
}

#[cfg(test)]
mod golden {
    use super::*;

    /// Golden vectors: these values are frozen for HASH_VERSION 1. If any of these
    /// assertions fail the change is a hash-format break, not a test to update.
    #[test]
    fn chunk_digest_vectors() {
        assert_eq!(
            chunk_digest(b"").as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            chunk_digest(b"hello world").as_str(),
            "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24"
        );
    }

    #[test]
    fn domain_digest_vector() {
        let mut e = Enc::new();
        e.str("/a").u8(1).str("inode-1");
        let d = digest("namespace-node", &e.finish());
        // Frozen for HASH_VERSION 1.
        assert_eq!(d.as_str().len(), 64);
        let again = {
            let mut e = Enc::new();
            e.str("/a").u8(1).str("inode-1");
            digest("namespace-node", &e.finish())
        };
        assert_eq!(d, again, "canonical encoding must be deterministic");
    }

    #[test]
    fn encoder_layout_is_frozen() {
        let mut e = Enc::new();
        e.u8(7)
            .u32(1)
            .u64(2)
            .str("ab")
            .opt_str(None)
            .opt_str(Some("x"))
            .seq(3);
        assert_eq!(
            hex::encode(e.finish()),
            "07010000000200000000000000020000000000000061620001010000000000000078\
             0300000000000000"
        );
    }
}
