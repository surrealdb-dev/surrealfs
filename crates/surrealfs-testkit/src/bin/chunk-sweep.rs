//! Chunk-size sweep: what a chunk size actually costs.
//!
//! The production value is 256 KiB and was, until this ran, an assumption. AgentFS uses 4 KiB and
//! `dofs` uses 512 KiB, so the plausible range spans two orders of magnitude and the three systems
//! disagree — which means somebody's default is wrong for somebody's workload.
//!
//! **This measures bytes, not seconds.** Wall-clock on a laptop is dominated by the storage engine,
//! the page cache, and whatever else the machine is doing; run it twice and you get two answers.
//! The quantity a chunk size actually controls is deterministic: how many bytes have to be
//! re-persisted when a file changes. Counting it is reproducible, comparable across machines, and
//! the thing that decides the trade-off. Timing belongs in a durability-normalised benchmark
//! against the pinned baselines, which is a separate exercise and must not be conflated with this
//! one — AgentFS runs with `synchronous = OFF`, so a naive timing comparison measures its missing
//! durability rather than its chunk size.
//!
//! Run: `cargo run -p surrealfs-testkit --bin chunk-sweep [--release]`

use std::collections::HashSet;

use surrealfs_content::chunk_bytes_with;
use surrealfs_types::ChunkDigest;

/// Sizes worth comparing: AgentFS's, two intermediates, ours, and `dofs`'s.
const SIZES: &[usize] = &[
    4 * 1024,
    16 * 1024,
    64 * 1024,
    256 * 1024,
    512 * 1024,
    1024 * 1024,
];

/// A deterministic pseudo-random body. Not `rand`: the sweep must give the same answer on every
/// machine and every run, or comparing two runs means nothing.
fn body(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn digests(bytes: &[u8], size: usize) -> HashSet<ChunkDigest> {
    chunk_bytes_with(bytes, size)
        .1
        .into_iter()
        .map(|c| c.digest)
        .collect()
}

/// Bytes that must be newly persisted to go from `before` to `after`.
fn new_bytes(before: &[u8], after: &[u8], size: usize) -> u64 {
    let held = digests(before, size);
    chunk_bytes_with(after, size)
        .1
        .into_iter()
        .filter(|c| !held.contains(&c.digest))
        .map(|c| c.bytes.len() as u64)
        .sum()
}

fn kib(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{} KiB", bytes / 1024)
    }
}

fn main() {
    const FILE: usize = 4 * 1024 * 1024;
    let original = body(FILE, 1);

    // One byte changed in the middle: the edit an agent makes constantly.
    let mut one_byte = original.clone();
    one_byte[FILE / 2] ^= 0xFF;

    // A line inserted near the top, shifting everything after it. This is the case fixed-size
    // chunking handles worst, and it is worth showing rather than hiding: every chunk after the
    // insertion point realigns, so the whole tail is rewritten regardless of chunk size.
    let mut inserted = Vec::with_capacity(FILE + 64);
    inserted.extend_from_slice(&original[..4096]);
    inserted.extend_from_slice(b"// a line inserted near the top of the file\n");
    inserted.extend_from_slice(&original[4096..]);

    // Appending, as a log or a build artifact does.
    let mut appended = original.clone();
    appended.extend_from_slice(&body(64 * 1024, 2));

    println!("Chunk-size sweep over a {} file", kib(FILE));
    println!("Deterministic byte counts, not timings. Lower is better except for `chunks`.\n");
    println!(
        "{:>9} │ {:>7} │ {:>12} │ {:>12} │ {:>12}",
        "chunk", "chunks", "1-byte edit", "insert @4KiB", "append 64KiB"
    );
    println!("{:─>10}┼{:─>9}┼{:─>14}┼{:─>14}┼{:─>14}", "", "", "", "", "");

    for &size in SIZES {
        let chunks = chunk_bytes_with(&original, size).1.len();
        let edit = new_bytes(&original, &one_byte, size);
        let insert = new_bytes(&original, &inserted, size);
        let append = new_bytes(&original, &appended, size);
        println!(
            "{:>9} │ {:>7} │ {:>12} │ {:>12} │ {:>12}",
            kib(size),
            chunks,
            kib(edit as usize),
            kib(insert as usize),
            kib(append as usize)
        );
    }

    println!("\nWhat the columns mean:");
    println!("  chunks       metadata per file — extents stored in the tree entry");
    println!("  1-byte edit  bytes re-persisted for a single changed byte: exactly one chunk");
    println!("  insert       fixed-size chunking realigns, so the tail is rewritten whatever");
    println!("               the size — content-defined chunking is the fix, and is not built");
    println!("  append       bytes re-persisted when data is added at the end");

    // The append column is the one that argues against very large chunks: appending to a file
    // rewrites the final partial chunk, so a 1 MiB chunk can cost 1 MiB to add one line.
    println!("\nRead the trade-off as: small chunks cost metadata, large chunks cost every edit.");
}
