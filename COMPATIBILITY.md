# SurrealFS compatibility manifest

Status: authoritative record of the pinned engine, toolchain, and format versions.
Update this file and re-run the engine-pin revalidation checklist in `RUST_SDK_PLAN.md`
whenever any row changes.

## Engine pin

| Item | Value |
|---|---|
| SurrealDB source | `/Users/kfarhan/workspace/surrealdb/surrealdb-private` (private; tag before external demo) |
| Branch | `arriqaaq/skv0213` |
| Commit | `4bbf01ed90deafb4849f5b4a09df1a1d90c0d31f` (2026-08-05) |
| SDK crate | `surrealdb` at `<pin>/surrealdb`, version `3.3.0-nightly` |
| SurrealKV | `0.21.3` (via `kv-surrealkv` feature) |
| Enabled features | `kv-mem` (tests), `kv-surrealkv` (persistent) |
| Forbidden imports | `surrealdb-engine-api`, `surrealdb-engine-local`, `surrealdb-datastore`, `surrealdb-kvs*`, `surrealkv` (direct) |
| Durability profile | sync every acknowledged commit |

## Engine ownership

SurrealDB and SurrealKV are ours to change. Both are checked out locally, and SurrealKV is
already modified here routinely — sibling checkouts carry group-commit, flush, and concurrency
work.

| Item | Value |
|---|---|
| SurrealKV checkout | `/Users/kfarhan/workspace/surrealdb/surrealkv` @ `ddf9576` ("Release 0.21.3") |
| Matches the pinned version | Yes — 0.21.3, so a local patch needs no version negotiation |
| Currently wired in? | No. `surrealdb-private` resolves `surrealkv = "0.21.3"` from crates.io and has no `[patch]` section. |
| To use the local copy | Add to `surrealdb-private/Cargo.toml`: `[patch.crates-io]` / `surrealkv = { path = "/Users/kfarhan/workspace/surrealdb/surrealkv" }` |

Record the effective block size and cache settings before and after any engine change, so a
tuning claim can be attributed to the change that caused it.

## Toolchain

| Item | Value |
|---|---|
| Rust channel (engine pin) | 1.95 (`rust-toolchain.toml` in the pin) |
| Rust used for SurrealFS | 1.96.0 |

## SurrealFS format versions

| Item | Value |
|---|---|
| Schema version | 1 (`schema/migrations/0001-core.surql`) |
| Root format version | 1 (canonical CBOR-free encoding, see `surrealfs-types::canonical`) |
| Hash version | 1 (BLAKE3-256, hex lowercase) |
| Export version | not yet defined (Phase 9) |

## Known gaps at this pin (owned upstream work)

- SurrealKV configuration is reachable but stringly-typed: it is set through connection query
  parameters (`surrealkv_block_size`, `surrealkv_block_cache_capacity`,
  `surrealkv_max_memtable_size`, `surrealkv_vlog_*`, `surrealkv_grouped_commit_*`), with no
  compile-time checking and no way to read back the effective values. A typed API is owned work,
  not a blocker — the knobs themselves are available today.
- Oversized-transaction failures are generic, not typed.
- No awaited, error-reporting public `shutdown()`; `Drop` is best-effort.
- No complete at-rest encryption/key lifecycle.

These are release gates for the phases named in `RUST_SDK_PLAN.md`, not reasons to add a second
store.
