//! Content chunking, hashing, verification, and the persistent namespace tree.
//!
//! Fixed-size chunking: simple, deterministic, and sufficient for now. Content-defined
//! chunking is a later, versioned change.
//!
//! The [`tree`] module holds the content-addressed directory tree that makes a commit cost
//! O(changed paths) rather than O(tree size).

pub mod tree;

use surrealfs_types::canonical::chunk_digest;
use surrealfs_types::state::Extent;
use surrealfs_types::{ChunkDigest, SfsError};

/// Fixed chunk size for ROOT_FORMAT_VERSION 1.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// A staged chunk: content-addressed bytes not yet referenced by any commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChunk {
    pub digest: ChunkDigest,
    pub bytes: Vec<u8>,
}

/// Split bytes into fixed-size chunks, returning the extent list and the staged chunks.
/// The extent list always covers the full byte range in order, with no holes.
pub fn chunk_bytes(bytes: &[u8]) -> (Vec<Extent>, Vec<StagedChunk>) {
    chunk_bytes_with(bytes, CHUNK_SIZE)
}

/// Chunk at an arbitrary size.
///
/// Exposed so a benchmark can sweep the size without rebuilding the crate against a different
/// constant — the production value stays [`CHUNK_SIZE`], and a root written at one size is not
/// interchangeable with a root written at another, which is why this is a measurement tool and
/// not a per-repository setting.
pub fn chunk_bytes_with(bytes: &[u8], chunk_size: usize) -> (Vec<Extent>, Vec<StagedChunk>) {
    assert!(chunk_size > 0, "chunk size must be positive");
    let mut extents = Vec::new();
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    for piece in bytes.chunks(chunk_size) {
        let digest = ChunkDigest(chunk_digest(piece));
        extents.push(Extent {
            file_offset: offset,
            length: piece.len() as u64,
            chunk: digest.clone(),
        });
        offset += piece.len() as u64;
        chunks.push(StagedChunk {
            digest,
            bytes: piece.to_vec(),
        });
    }
    (extents, chunks)
}

/// Verify a chunk's bytes against its claimed digest.
pub fn verify_chunk(digest: &ChunkDigest, bytes: &[u8]) -> Result<(), SfsError> {
    let actual = ChunkDigest(chunk_digest(bytes));
    if &actual == digest {
        Ok(())
    } else {
        Err(SfsError::Corruption(format!(
            "chunk digest mismatch: expected {digest}, computed {actual}"
        )))
    }
}

/// Reassemble file bytes from ordered extents and a chunk fetcher.
pub fn assemble<F>(extents: &[Extent], mut fetch: F) -> Result<Vec<u8>, SfsError>
where
    F: FnMut(&ChunkDigest) -> Result<Vec<u8>, SfsError>,
{
    let mut out = Vec::new();
    for ext in extents {
        if ext.file_offset != out.len() as u64 {
            return Err(SfsError::Corruption(format!(
                "extent offset {} does not match assembled length {}",
                ext.file_offset,
                out.len()
            )));
        }
        let bytes = fetch(&ext.chunk)?;
        verify_chunk(&ext.chunk, &bytes)?;
        if bytes.len() as u64 != ext.length {
            return Err(SfsError::Corruption(format!(
                "extent length {} does not match chunk length {}",
                ext.length,
                bytes.len()
            )));
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn roundtrip_multi_chunk() {
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        let (extents, chunks) = chunk_bytes(&data);
        assert_eq!(extents.len(), 3);
        let store: HashMap<_, _> = chunks
            .into_iter()
            .map(|c| (c.digest.clone(), c.bytes))
            .collect();
        let rebuilt = assemble(&extents, |d| {
            store
                .get(d)
                .cloned()
                .ok_or_else(|| SfsError::NotFound(d.to_string()))
        })
        .unwrap();
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn empty_file_has_no_extents() {
        let (extents, chunks) = chunk_bytes(b"");
        assert!(extents.is_empty());
        assert!(chunks.is_empty());
        assert_eq!(
            assemble(&[], |_| -> Result<Vec<u8>, SfsError> { unreachable!() }).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn corruption_is_detected() {
        let (extents, chunks) = chunk_bytes(b"data");
        let err = assemble(&extents, |_| Ok(b"tampered!".to_vec()));
        assert!(err.is_err());
        assert!(verify_chunk(&chunks[0].digest, b"data").is_ok());
        assert!(verify_chunk(&chunks[0].digest, b"datX").is_err());
    }
}
