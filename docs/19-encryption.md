# Content encryption

## What it protects, and what it does not

Chunk bodies are sealed with AES-256-GCM at the store boundary. **File content and KV values are
unreadable without the key.** Everything else is not:

| Encrypted | In the clear |
|---|---|
| file bytes | paths and file names |
| KV values | file sizes |
| | commit messages |
| | tool-call names and inputs |
| | branch and savepoint names |
| | timestamps and commit graph |

That division follows from where chunks sit: content lives in chunk rows, and structure lives in
tree nodes and commit records. It is asserted by `metadata_is_deliberately_not_encrypted`, so it
stays a known property rather than becoming a surprise — and so that a future change to it has to
be deliberate.

Encrypting the rest means a cipher layer inside SurrealKV, which has no crypto at all today. That
is scheduled upstream work in code this project owns, not a limitation to design around. Until
then, **do not describe a repository as "encrypted" without saying which half**.

## Using it

```
export SURREALFS_KEY=$(openssl rand -hex 32)
surrealfs --repo mine init .
```

The key is 32 bytes as 64 hex characters. A `--key` flag exists and the environment variable is
the documented path, because an argument is visible to every user on the machine through `ps` and
is kept in shell history. (AgentFS ships the argv form as its primary interface and derives
`Debug` on the struct holding the key; neither is copied here — `ChunkKey` prints as
`ChunkKey(redacted)` and is zeroized on drop.)

There is no key derivation. A passphrase would need a KDF and a salt, and getting that wrong is
worse than not offering it; the parity baseline does not offer one either.

## Failure modes, all typed

| Repository | Key | Result |
|---|---|---|
| encrypted | none | refused at open: "supply --key or set SURREALFS_KEY" |
| plaintext | supplied | refused at open |
| encrypted | wrong | `Encryption` on first read, never `Corruption` |

Refusing a key on a plaintext repository is the important one: it stops someone believing their
data is encrypted when it is not.

A wrong key and tampered bytes fail identically inside AES-GCM, so the two are separated by error
type rather than by symptom. `SfsError::Encryption` maps to `EACCES` on a mount — a permission
problem, not a missing file and not a broken one.

## Design notes

**Digests stay plaintext BLAKE3.** They are computed upstream of the store, so encryption is
invisible to identity: the same workload produces the same state root encrypted or not, dedup
still works, and an archive moves between an encrypted repository and a plaintext one in either
direction.

The cost is real and worth stating: **anyone holding the database can test a guessed plaintext
against the stored digest.** Keying the digest would close that and break dedup, identity, and the
frozen golden vectors with it. This is a deliberate trade, not an oversight.

**The nonce is random per seal**, so identical plaintext stores differently every time. Dedup is
unaffected because it keys on the digest, never on stored bytes.

**The digest is the AEAD associated data.** Swapping two chunk rows produces an authentication
failure rather than a file that silently reads as someone else's content.

**`length` records the plaintext length.** GC reports reclaimed bytes from it, and a figure
describing the envelope would answer a question nobody asked. Ciphertext is `length + 28`.

## Archives are plaintext

`read_archive` verifies every body against its BLAKE3 digest, and that digest is over plaintext,
so an archive of ciphertext could never be imported anywhere — including back into the repository
that produced it. Export therefore decrypts.

Two consequences, neither hidden: **exporting an encrypted repository needs the key**, and **an
archive file is as sensitive as the data in it.**
