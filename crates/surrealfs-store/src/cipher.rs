//! Encrypting chunk bodies.
//!
//! This encrypts **content**, not the repository. File bytes and KV values become unreadable
//! without the key; paths, file sizes, commit messages, and tool-call inputs stay in plaintext,
//! because they live in tree nodes and commit records rather than in chunks. That is a real
//! limit rather than an oversight, and it is asserted by a test so nobody later assumes
//! otherwise. Encrypting the whole database means adding a cipher layer inside SurrealKV, which
//! is scheduled upstream work.
//!
//! Digests stay a plaintext BLAKE3 over plaintext bytes, computed upstream in the content layer
//! before anything reaches the store. So encryption is invisible to identity: the same workload
//! produces the same state root encrypted or not, dedup still works, and an archive moves
//! between an encrypted repository and a plaintext one. The cost of that choice is stated
//! honestly in `docs/` — anyone holding the database can test a guessed plaintext against the
//! stored digest. Keying the digest would close that and break dedup, identity, and the frozen
//! golden vectors along with it.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use surrealfs_types::{ChunkDigest, SfsError};
use zeroize::Zeroizing;

/// Marker written to `repository.encryption`, so a future cipher change is detectable rather
/// than silently misread.
pub const ENCRYPTION_MARKER: &str = "aes-256-gcm/v1";

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Bytes an encrypted body adds over its plaintext.
pub const ENVELOPE_LEN: usize = NONCE_LEN + TAG_LEN;

/// A 32-byte content key.
///
/// Deliberately not `#[derive(Debug)]`. AgentFS derives `Debug` on its key struct and on the
/// options struct holding it, so any debug-log of its configuration prints the raw key; the
/// manual impl below makes that impossible here. The key is wiped on drop by `Zeroizing`.
pub struct ChunkKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for ChunkKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChunkKey(redacted)")
    }
}

impl ChunkKey {
    /// Parse 64 hex characters into a key.
    ///
    /// A wrong length is refused rather than padded or truncated: a key that silently becomes a
    /// different key encrypts data nobody can later decrypt, and the error names the expected
    /// length so the fix is obvious.
    pub fn from_hex(hex: &str) -> Result<Self, SfsError> {
        let hex = hex.trim();
        let bytes = hex::decode(hex).map_err(|_| {
            SfsError::Encryption("the key must be hexadecimal characters only".into())
        })?;
        let len = bytes.len();
        let array: [u8; 32] = bytes.try_into().map_err(|_| {
            SfsError::Encryption(format!(
                "the key must be 32 bytes (64 hex characters), got {len} bytes"
            ))
        })?;
        Ok(ChunkKey(Zeroizing::new(array)))
    }
}

/// Key material on its way from a caller to a [`Store`](crate::Store).
///
/// `ChunkKey` is deliberately neither `Clone` nor `Debug`, which is right for the thing doing
/// the encrypting but wrong for an options struct that gets copied and logged. This carries the
/// hex form with the same redaction guarantee, zeroized on drop, and is validated on
/// construction so a bad key fails where it is supplied rather than at first use.
#[derive(Clone)]
pub struct KeyMaterial(Zeroizing<String>);

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyMaterial(redacted)")
    }
}

impl KeyMaterial {
    pub fn parse(hex: &str) -> Result<Self, SfsError> {
        // Validate now and discard the result: the point is to reject a bad key at the boundary
        // where the user can still see which argument was wrong.
        ChunkKey::from_hex(hex)?;
        Ok(KeyMaterial(Zeroizing::new(hex.trim().to_string())))
    }

    pub fn key(&self) -> Result<ChunkKey, SfsError> {
        ChunkKey::from_hex(&self.0)
    }
}

/// Seals and opens chunk bodies.
pub struct ChunkCipher {
    inner: Aes256Gcm,
}

impl std::fmt::Debug for ChunkCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChunkCipher(aes-256-gcm)")
    }
}

impl ChunkCipher {
    pub fn new(key: &ChunkKey) -> Self {
        ChunkCipher {
            inner: Aes256Gcm::new(key.0.as_slice().into()),
        }
    }

    /// Encrypt a body as `nonce || ciphertext || tag`.
    ///
    /// The nonce is random per call, so encrypting identical plaintext twice gives different
    /// stored bytes. That does not weaken dedup, because dedup keys on the plaintext digest and
    /// never on what is stored — an already-present digest is never re-sealed.
    ///
    /// The chunk's digest is the associated data. That binds a body to its identity: swapping
    /// two chunk rows in the database produces an authentication failure rather than a file that
    /// silently reads as someone else's content.
    pub fn seal(&self, digest: &ChunkDigest, plaintext: &[u8]) -> Result<Vec<u8>, SfsError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom_bytes(&mut nonce)?;
        let sealed = self
            .inner
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: digest.as_str().as_bytes(),
                },
            )
            .map_err(|_| SfsError::Encryption("could not encrypt a chunk body".into()))?;

        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Decrypt a body sealed by [`ChunkCipher::seal`].
    ///
    /// A failure here is almost always the wrong key, so it is reported as an encryption error.
    /// Reporting it as corruption would send someone hunting an integrity bug that does not
    /// exist.
    pub fn open(&self, digest: &ChunkDigest, stored: &[u8]) -> Result<Vec<u8>, SfsError> {
        if stored.len() < ENVELOPE_LEN {
            return Err(SfsError::Encryption(format!(
                "an encrypted body needs at least {ENVELOPE_LEN} bytes, got {}",
                stored.len()
            )));
        }
        let (nonce, body) = stored.split_at(NONCE_LEN);
        self.inner
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: digest.as_str().as_bytes(),
                },
            )
            .map_err(|_| {
                SfsError::Encryption(
                    "could not decrypt a chunk body; the key is wrong for this repository".into(),
                )
            })
    }
}

/// Fill a buffer with random bytes.
///
/// `aes-gcm` re-exports an OS RNG through its `aead` dependency, which avoids taking `rand` as a
/// direct dependency just for twelve bytes.
fn getrandom_bytes(buf: &mut [u8]) -> Result<(), SfsError> {
    use aes_gcm::aead::rand_core::RngCore;
    aes_gcm::aead::OsRng
        .try_fill_bytes(buf)
        .map_err(|e| SfsError::Encryption(format!("could not read random bytes: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(bytes: &[u8]) -> ChunkDigest {
        ChunkDigest(surrealfs_types::canonical::chunk_digest(bytes))
    }

    fn key() -> ChunkKey {
        ChunkKey::from_hex(&"ab".repeat(32)).unwrap()
    }

    #[test]
    fn a_sealed_body_opens_back_to_the_original() {
        let cipher = ChunkCipher::new(&key());
        let plaintext = b"the quick brown fox";
        let d = digest_of(plaintext);
        let sealed = cipher.seal(&d, plaintext).unwrap();
        assert_ne!(sealed, plaintext, "the body was stored in the clear");
        assert_eq!(cipher.open(&d, &sealed).unwrap(), plaintext);
    }

    /// The nonce is random, so identical plaintext seals differently every time. Dedup is
    /// unaffected because it keys on the plaintext digest, never on the stored bytes.
    #[test]
    fn sealing_the_same_bytes_twice_gives_different_stored_bytes() {
        let cipher = ChunkCipher::new(&key());
        let plaintext = b"repeated content";
        let d = digest_of(plaintext);
        let a = cipher.seal(&d, plaintext).unwrap();
        let b = cipher.seal(&d, plaintext).unwrap();
        assert_ne!(a, b, "the nonce is not varying");
        assert_eq!(cipher.open(&d, &a).unwrap(), plaintext);
        assert_eq!(cipher.open(&d, &b).unwrap(), plaintext);
    }

    /// The digest is the associated data, so a body moved to another chunk's row fails to open
    /// instead of reading as that chunk's content.
    #[test]
    fn a_body_cannot_be_opened_under_another_chunks_digest() {
        let cipher = ChunkCipher::new(&key());
        let plaintext = b"secret";
        let sealed = cipher.seal(&digest_of(plaintext), plaintext).unwrap();
        let other = digest_of(b"a different chunk entirely");
        assert!(matches!(
            cipher.open(&other, &sealed),
            Err(SfsError::Encryption(_))
        ));
    }

    #[test]
    fn the_wrong_key_reports_encryption_not_corruption() {
        let plaintext = b"secret";
        let d = digest_of(plaintext);
        let sealed = ChunkCipher::new(&key()).seal(&d, plaintext).unwrap();

        let wrong = ChunkKey::from_hex(&"cd".repeat(32)).unwrap();
        let err = ChunkCipher::new(&wrong).open(&d, &sealed).unwrap_err();
        assert!(
            matches!(err, SfsError::Encryption(_)),
            "a wrong key must not look like corruption: {err:?}"
        );
    }

    #[test]
    fn a_truncated_body_is_refused_rather_than_panicking() {
        let cipher = ChunkCipher::new(&key());
        assert!(matches!(
            cipher.open(&digest_of(b"x"), &[0u8; 4]),
            Err(SfsError::Encryption(_))
        ));
    }

    #[test]
    fn keys_must_be_exactly_thirty_two_bytes() {
        assert!(ChunkKey::from_hex(&"ab".repeat(32)).is_ok());
        // Too short, too long, and not hex at all.
        for bad in [
            "ab",
            &"ab".repeat(31),
            &"ab".repeat(33),
            "zz".repeat(32).as_str(),
        ] {
            assert!(
                ChunkKey::from_hex(bad).is_err(),
                "accepted a bad key: {bad:?}"
            );
        }
    }

    /// A key that reaches a log is a key that has leaked.
    #[test]
    fn a_key_never_prints_itself() {
        let hex = "ab".repeat(32);
        let printed = format!("{:?}", ChunkKey::from_hex(&hex).unwrap());
        assert_eq!(printed, "ChunkKey(redacted)");
        assert!(!printed.contains("ab"), "the key leaked into Debug output");
    }

    #[test]
    fn the_envelope_costs_what_the_schema_comment_claims() {
        let cipher = ChunkCipher::new(&key());
        let plaintext = vec![7u8; 1000];
        let sealed = cipher.seal(&digest_of(&plaintext), &plaintext).unwrap();
        assert_eq!(sealed.len(), plaintext.len() + ENVELOPE_LEN);
    }
}
