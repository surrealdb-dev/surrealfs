//! JSON storage representation of state nodes.
//!
//! Node *identity* is always the canonical digest from `surrealfs-types` /
//! `surrealfs-content`; the JSON here is only what goes in `state_node.body`, and every load
//! re-derives the digest and compares. That keeps the storage encoding free to change without
//! ever becoming the source of truth for identity.

use serde::{Deserialize, Serialize};
use surrealfs_content::tree::DirNode;
use surrealfs_types::state::KvMap;
use surrealfs_types::{ChunkDigest, SfsError, StateNodeId};

/// KV is serialised as an explicit entry list; a JSON object cannot key on a tuple.
#[derive(Serialize, Deserialize)]
struct KvBody(Vec<(String, String, ChunkDigest)>);

fn ser<T: Serialize>(value: &T) -> Result<String, SfsError> {
    serde_json::to_string(value).map_err(|e| SfsError::Storage(format!("encode node body: {e}")))
}

fn de<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, SfsError> {
    serde_json::from_str(json).map_err(|e| SfsError::Corruption(format!("decode node body: {e}")))
}

pub fn encode_dir(node: &DirNode) -> Result<String, SfsError> {
    ser(node)
}

/// Decode a directory node and verify it hashes to the id it was stored under.
pub fn decode_dir(id: &StateNodeId, json: &str) -> Result<DirNode, SfsError> {
    let node: DirNode = de(json)?;
    let actual = node.digest();
    if &actual != id {
        return Err(SfsError::Corruption(format!(
            "tree node {id} decoded to digest {actual}"
        )));
    }
    Ok(node)
}

pub fn encode_kv(kv: &KvMap) -> Result<String, SfsError> {
    ser(&KvBody(
        kv.iter()
            .map(|((ns, key), v)| (ns.clone(), key.clone(), v.clone()))
            .collect(),
    ))
}

/// Decode the KV node without verifying it.
///
/// Callers that hold the node's own id should prefer [`decode_kv`]. `load_root` instead
/// verifies by re-deriving the whole state root from what it loaded, which subsumes a
/// per-node check.
pub fn parse_kv(json: &str) -> Result<KvMap, SfsError> {
    let body: KvBody = de(json)?;
    Ok(body
        .0
        .into_iter()
        .map(|(ns, key, v)| ((ns, key), v))
        .collect())
}

/// Decode the KV node and verify it hashes to the id it was stored under.
pub fn decode_kv(id: &StateNodeId, json: &str) -> Result<KvMap, SfsError> {
    let kv = parse_kv(json)?;
    let actual = surrealfs_types::state::kv_digest(&kv);
    if &actual != id {
        return Err(SfsError::Corruption(format!(
            "kv node {id} decoded to digest {actual}"
        )));
    }
    Ok(kv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealfs_content::tree::{Entry, Meta};
    use surrealfs_types::canonical::chunk_digest;
    use surrealfs_types::state::Extent;

    #[test]
    fn dir_node_roundtrips_and_verifies() {
        let mut node = DirNode::default();
        node.entries.insert(
            "f.txt".into(),
            Entry::File {
                meta: Meta::file(),
                size: 1,
                links: Vec::new(),
                extents: vec![Extent {
                    file_offset: 0,
                    length: 1,
                    chunk: ChunkDigest(chunk_digest(b"x")),
                }],
            },
        );
        let id = node.digest();
        let json = encode_dir(&node).unwrap();
        assert_eq!(decode_dir(&id, &json).unwrap(), node);

        // A body that does not match its id is corruption, not a decode error.
        let other = DirNode::default();
        let bad = encode_dir(&other).unwrap();
        assert!(matches!(
            decode_dir(&id, &bad),
            Err(SfsError::Corruption(_))
        ));
    }

    #[test]
    fn kv_roundtrips_and_verifies() {
        let mut kv = KvMap::new();
        kv.insert(("n".into(), "k".into()), ChunkDigest(chunk_digest(b"v")));
        let id = surrealfs_types::state::kv_digest(&kv);
        let json = encode_kv(&kv).unwrap();
        assert_eq!(decode_kv(&id, &json).unwrap(), kv);
        assert!(matches!(
            decode_kv(&id, &encode_kv(&KvMap::new()).unwrap()),
            Err(SfsError::Corruption(_))
        ));
    }
}
