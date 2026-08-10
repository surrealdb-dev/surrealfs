-- Chunk bodies may be stored encrypted.
--
-- The `digest` column stays a plaintext BLAKE3 of the plaintext bytes, so identity, dedup, state
-- roots, and commit ids are all unaffected by whether a body happens to be encrypted. The same
-- workload produces the same state root in an encrypted repository as in a plaintext one, which
-- is what lets an archive move between them.
--
-- `length` likewise stays the *plaintext* length. GC reports reclaimed bytes from this column,
-- and a figure describing the envelope rather than the data would be a worse answer to the
-- question people actually ask. Ciphertext length is length + 28 (12-byte nonce, 16-byte tag).
DEFINE FIELD OVERWRITE storage_kind ON TABLE chunk TYPE string
    ASSERT $value IN ["INLINE", "ENCRYPTED"];

-- Recorded once per repository, so opening one without its key fails immediately and with a
-- typed error, rather than as a decryption failure on whatever read happens to come first.
DEFINE FIELD encryption ON TABLE repository TYPE option<string>;
