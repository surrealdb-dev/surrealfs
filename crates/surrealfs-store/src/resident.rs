//! The resident state tier: a bounded, process-lifetime cache of tree nodes.
//!
//! Every read path used to build a fresh node map and drop it, so resolving a depth-six path
//! cost six sequential queries *every time* — including for the directories near the root that
//! every single lookup touches. That is affordable for an SDK call and not affordable for a
//! mount, where `getattr` and `lookup` dominate.
//!
//! What makes this cheap here is that nodes are immutable and digest-keyed: the cache key *is*
//! the content hash, so a cached node can never be stale and the cache needs **no invalidation
//! logic at all**. Contrast the transaction-aware caches a mutable-row design would require.
//!
//! It is a cache and never truth. Dropping it costs latency and nothing else, which is what
//! separates this from keeping the authoritative tree in memory: ContextFS does the latter and
//! loses everything since the last checkpoint on a crash.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use surrealfs_content::tree::DirNode;
use surrealfs_types::StateNodeId;

/// Default ceiling on resident nodes. Nodes are small (a directory's entry list), so this is
/// tens of megabytes at worst, and a repository's hot set is far smaller than its total.
pub const DEFAULT_CAPACITY: usize = 8192;

/// Hit and miss counts, so a benchmark can attribute a latency change to this cache rather
/// than to engine tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub resident: usize,
}

/// A digest-keyed node cache shared by every reader of one store.
pub struct ResidentNodes {
    inner: Mutex<Inner>,
    hits: AtomicU64,
    misses: AtomicU64,
    capacity: usize,
}

struct Inner {
    nodes: HashMap<StateNodeId, DirNode>,
    /// Insertion order, for eviction. Nodes are interchangeable once cached — any of them can
    /// be re-fetched — so the eviction policy only has to be cheap, not clever.
    order: std::collections::VecDeque<StateNodeId>,
}

impl ResidentNodes {
    pub fn new(capacity: usize) -> Self {
        ResidentNodes {
            inner: Mutex::new(Inner {
                nodes: HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            capacity: capacity.max(1),
        }
    }

    pub fn get(&self, id: &StateNodeId) -> Option<DirNode> {
        let found = self
            .inner
            .lock()
            .expect("resident cache mutex poisoned")
            .nodes
            .get(id)
            .cloned();
        match &found {
            Some(_) => self.hits.fetch_add(1, Ordering::Relaxed),
            None => self.misses.fetch_add(1, Ordering::Relaxed),
        };
        found
    }

    pub fn insert(&self, id: StateNodeId, node: DirNode) {
        let mut inner = self.inner.lock().expect("resident cache mutex poisoned");
        if inner.nodes.contains_key(&id) {
            return;
        }
        while inner.nodes.len() >= self.capacity {
            match inner.order.pop_front() {
                Some(oldest) => {
                    inner.nodes.remove(&oldest);
                }
                None => break,
            }
        }
        inner.order.push_back(id.clone());
        inner.nodes.insert(id, node);
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            resident: self
                .inner
                .lock()
                .expect("resident cache mutex poisoned")
                .nodes
                .len(),
        }
    }

    /// Drop everything. Safe at any moment — this is a cache, not state.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("resident cache mutex poisoned");
        inner.nodes.clear();
        inner.order.clear();
    }
}

impl Default for ResidentNodes {
    fn default() -> Self {
        ResidentNodes::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> DirNode {
        let mut node = DirNode::default();
        node.entries.insert(
            name.to_string(),
            surrealfs_content::tree::Entry::Dir {
                meta: surrealfs_content::tree::Meta::dir(),
                node: surrealfs_content::tree::empty_root(),
            },
        );
        node
    }

    #[test]
    fn caches_by_digest_and_counts_hits() {
        let cache = ResidentNodes::new(16);
        let n = node("a");
        let id = n.digest();

        assert!(cache.get(&id).is_none());
        cache.insert(id.clone(), n.clone());
        assert_eq!(cache.get(&id), Some(n));

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.resident, 1);
    }

    #[test]
    fn evicts_when_full_without_losing_correctness() {
        let cache = ResidentNodes::new(2);
        let ids: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                let n = node(name);
                let id = n.digest();
                cache.insert(id.clone(), n);
                id
            })
            .collect();

        assert_eq!(cache.stats().resident, 2, "capacity is respected");
        // The oldest was evicted; a miss is a re-fetch, never a wrong answer.
        assert!(cache.get(&ids[0]).is_none());
        assert!(cache.get(&ids[2]).is_some());
    }

    #[test]
    fn clearing_is_always_safe() {
        let cache = ResidentNodes::new(16);
        let n = node("a");
        cache.insert(n.digest(), n.clone());
        cache.clear();
        assert_eq!(cache.stats().resident, 0);
        assert!(cache.get(&n.digest()).is_none());
    }
}
