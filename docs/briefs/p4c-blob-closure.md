# Brief — P4c blob review closure

> **Source:** blind review of `9a9ea3f` · **After:** P4b · **Before:** P5b sync
> **Rule:** each fix ships with a test that fails without it.

## 1. MAJOR — a metadata patch can make stored bytes unreadable

`replace_attachments_in_tx` preserves the `blob_hash` of a surviving attachment id but writes the
caller's `size`. A draft patch with a stale or zero size leaves `attachments.size` smaller than the
blob, and the read cap (`size + slack`) rejects the real bytes with `BlobTooLarge`. `has_attachments`
stays true and there is no recovery path.

**Fix.** When an attachment id survives and has a stored hash, the row's `size` is the stored byte
count, not the caller's metadata: preserve both the hash and the size. If the caller's size disagrees,
keep the stored one (the bytes are canonical) and document it. When there is no stored hash, use the
caller's size as today. Test: create a draft with size 0, store 2 MiB, patch the draft with the same
id and size 0, reopen, read the bytes back.

## 2. MINOR — dedup trusts a corrupt or foreign file and cannot heal it

`put` returns success for any regular file at the hash path, without authenticating it. A corrupt
file stays corrupt forever and the error text tells the user to re-sync, which is a no-op.

**Fix.** On an existing file, authenticate it (bounded read) before declaring dedup success. If it is
missing/corrupt/undecryptable, replace it atomically with the new bytes. Keep the fast path when the
file authenticates. Test: corrupt a stored blob, `put` the same bytes, then `get` succeeds; a second
key in the same directory cannot make `put` report success for bytes it cannot read.

Also change the blob root to be derived from the database **file** (`mail.db` → `mail.db.blobs/`),
not the containing folder, so two stores never share a blob namespace. Update the store README,
architecture and tests. Document that moving a store means moving its `.blobs` directory.

## 3. MINOR — `get` does not verify the content hash

The AAD binds the hash, but `get` never recomputes BLAKE3 over the decrypted bytes. A writer with the
subkey can plant content under a false name and consumers trust the name.

**Fix.** After authentication and decompression, recompute BLAKE3 and compare with the requested
hash; a mismatch is `CorruptBlob`. Test: seal a payload with the blob subkey and AAD for hash A, write
it at path B, `get(B)` fails.

## 4. MINOR — the rename is not durable

`write_atomic` fsyncs the temp file and renames, but never persists the directory entry; the row is
committed after the rename. On Unix, fsync the shard directory after the rename. On Windows there is
no portable directory fsync without Win32 calls; open the final path and `sync_all` it, document the
remaining Windows window, and do not invent unsafe FFI. Add a comment and, on Unix, a test that the
directory fsync path is exercised (no power-loss simulation).

## 5. MINOR — the AAD binding of flags/length is untested

The tamper test flips only tag/ciphertext/nonce and an over-cap length that is rejected before the
AAD. Add: flip the `flags` byte (set the compressed bit) and a length inside the cap; both must be
`CorruptBlob` because the AAD no longer matches. Add an uncompressed payload with the compressed bit
set and a compressed payload with the bit cleared.

## 6. NIT — Windows traversal forms untested

Add `C:\...`, `\\?\C:\...`, `\\server\share`, `name:stream`, trailing dots/spaces and an 8.3 name to
the rejected-hash corpus. The validator is already a 64-lowercase-hex check; prove it.

## 7. NIT — the no-plaintext scan runs after the store closed

Scan the database, WAL, SHM and every blob **while the store is open**, with the blob canary and the
key as needles, then close. Update the comment that claims WAL coverage.

## 8. NIT — HKDF state is not zeroised

The `Hkdf` value holds the PRK-derived HMAC state and drops without zeroisation. Do not hand-roll
HMAC; add it to the documented "copies outside our reach" list in `crates/crypto` docs/README and
`docs/threat-model.md`, beside the cipher state and the rejected-plaintext buffer.

## 9. NIT — `BadHash` echoes the rejected input

`BadHash { hash }` formats up to 80 characters of the caller's string. Remove the value: report only
that an id is not a 64-character lowercase hex hash (and maybe its length). Update the tests and any
docs that promise no content in errors.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests. Update
`crates/store/README.md`, `crates/crypto/README.md`, `docs/architecture.md` §4.5,
`docs/threat-model.md` §4.3 and `docs/store-spike.md` where the behaviour or the inventory changed.
Temp files only in `%TEMP%\opencode\p4c-postino`; do not commit.

## Report

Per finding: fixed or rejected with reason, the test that proves it, and the new blob layout.
