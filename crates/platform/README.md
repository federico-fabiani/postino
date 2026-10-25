# postino-platform

**Layer:** foundation. Depends on no other workspace crate; no UI toolkit.

Operating-system seams that every other crate must use instead of talking to the platform itself.
Today the crate owns one seam: the **store key**.

## Contract

- `load_or_create_key(scope) -> Result<StoreKey, PlatformError>` reads a 32-byte secret from the OS
  keyring (Windows Credential Manager: service `postino`, user = `scope`). On first use it generates
  the key with `getrandom` and stores it.
- The key is never logged, never formatted with `Debug` and never written to a file. `StoreKey`
  zeroises its bytes on drop, has no `Clone`, and exposes only `as_bytes()`.
- Every failure is an error: an unavailable vault, a read error, a secret that is not 32 bytes, a
  random-source failure. There is no derived key and no plaintext fallback. `PlatformError` is safe
  to display: no variant carries secret material.
- Scopes are 1-128 printable characters. Use a stable value (`"default"` for `postino --store`, an
  account id for a per-account store): a different scope is a different key and therefore a
  different database.
- The crate installs `windows-native-keyring-store` lazily on first use; on platforms without a
  supported native store it returns `PlatformError::Unsupported` rather than writing the key
  somewhere weaker.
- `postino-store` never reads a keyring. It accepts a `postino_store::StoreKey`; the app maps
  `platform::StoreKey::as_bytes()` into it. That separation is what lets the store tests run with a
  random in-memory key.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-platform
```

Every test installs `keyring_core::mock` (or a deliberately broken store) as the default store
before the first call, so the suite never reads or writes the real vault. Covered: creation is
idempotent, a reload and a fresh handle return the same 32 bytes, scopes are independent, a missing
vault and a read failure are readable errors, a short secret is refused, `Debug` is redacted.

The native Windows path is not part of the default suite. Before a release, run a manual smoke test
with a throwaway scope and delete the credential afterwards.

## Why the app does not call the keyring itself

One owner of the policy: which service name, which scope, what happens on failure and how the key is
wiped. A second implementation would eventually diverge, and a store crate that reads the vault could
not be tested without touching the user's credentials.
