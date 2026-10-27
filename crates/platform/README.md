# postino-platform

**Layer:** foundation. Depends on no other workspace crate; no UI toolkit.

Operating-system seams that every other crate must use instead of talking to the platform itself.
The crate owns two seams: the **store key** and **account credentials**.

## Contract

- `Keyring::system()` resolves the production lock path; `Keyring::at(path)` is the test/embedding
  form with an explicit path. Both expose the same operations.
- `load_or_create_key(scope) -> Result<StoreKey, PlatformError>` reads a 32-byte secret from the OS
  keyring (Windows Credential Manager: service `postino`, user = `scope`). On first use it generates
  the key with `getrandom` and stores it.
- `store_secret(scope, &[u8])`, `load_secret(scope) -> Option<Zeroizing<Vec<u8>>>` and
  `delete_secret(scope)` own every account credential. A write is read back (in a `Zeroizing`
  buffer) before it reports success; a missing secret is `Ok(None)`; deleting a missing secret is a
  success, so cleanup is retryable.
- Scopes are 1-128 printable characters and contain no control characters. Use `default` for the
  SQLCipher store key and `account:<id>:imap` for one account's IMAP password
  (`account_scope(id)` builds and validates the latter). A different scope is a different keyring
  entry; secret bytes are never stored under the store-key scope.
- The key is never logged, never formatted with `Debug` and never written to a file. `StoreKey`
  zeroises its bytes on drop, has no `Clone`, and exposes only `as_bytes()`. `load_secret` returns a
  `Zeroizing` buffer; callers must not format or clone it without the same guarantee.
- Every failure is an error: an unavailable vault, a read error, a secret that is not 32 bytes, a
  random-source failure. There is no derived key and no plaintext fallback. `PlatformError` is safe
  to display: no variant carries secret material.
- Every call takes an OS file lock at `<app local data>/Postino/keyring.lock` for the whole
  read/write/delete, including the first key read, so two processes cannot disagree about a secret.
  A key read-back mismatch adopts the stable 32-byte key the vault actually holds; a vault that keeps
  changing fails closed after one retry. A generic secret that does not read back is an error, never
  silently accepted.
- **The only native backend in this build is Windows Credential Manager**
  (`windows-native-keyring-store`). On macOS and Linux `install_native_store` returns
  `PlatformError::Unsupported`, so the store key and every account credential fail closed at this
  seam; there is no weaker file fallback. Their native keychains land with the packaging milestone.
- `postino-store` never reads a keyring. It accepts a `postino_store::StoreKey`; the app maps
  `platform::StoreKey::as_bytes()` into it. That separation is what lets the store tests run with a
  random in-memory key.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-platform
```

Every test installs `keyring_core::mock` (or a deliberately broken store) as the default store
before the first call, and passes its own temporary lock path, so the suite never reads or writes
the real vault or `%LOCALAPPDATA%\Postino`. Covered: creation is idempotent, a reload and a fresh
handle return the same 32 bytes, scopes are independent, account scopes stay independent across
write/read/delete, a missing vault and a read failure are readable errors, a short secret is refused,
`Debug` is redacted, the first-run lock is exclusive across two threads, an existing key is read only
under the lock, a foreign key is adopted after a read-back mismatch, and an unstable vault fails
closed after one retry. The secret round trip/update/delete tests also prove that a value which does
not read back is refused and that no error contains the bytes.

The native Windows path is not part of the default suite. Before a release, run a manual smoke test
with a throwaway scope and delete the credential afterwards.

## Tray, autostart and notifications

P10b1 adds three more seams behind the same rule: no crate outside this one calls the OS.

- [`tray::Tray`] owns a daemon tray icon and a bounded channel of `TrayAction`s (`Open`, `SyncNow`,
  `Quit`). `TrayState` is quiet, syncing, offline or failed; there is no badge count. `RecorderTray`
  is the headless implementation. `Win32Tray` lives in the one `#[allow(unsafe_code)]` module and
  uses a hidden window plus `Shell_NotifyIconW`; a missing Explorer is a normal `Unavailable` error.
- [`autostart::Autostart`] reads, enables and disables the per-user Run value
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Postino` with the exact quoted command from
  `autostart_command`. `RecorderAutostart` is the headless implementation. No service and no
  elevation; ADR-0006 records why this replaces Task Scheduler. Tests only write
  `HKCU\Software\Postino\Tests\<pid>-<counter>`.
- [`notify::Notifier`] shows one bounded `Notice` (title <= 63 chars, body <= 255, newlines and
  control characters stripped). `RecorderNotifier` records it. Previews are off by default, so the
  daemon sends a fixed sentence with no sender, subject or body; the daemon owns debounce and the
  scalar new-mail horizon, not this crate.
- [`atomic_replace`] writes a sibling file and replaces the target (`MoveFileExW` with write-through
  on Windows); the daemon uses it for `daemon.json`.

The real Win32 tray probe is opt-in with `POSTINO_WIN32_UI_TESTS=1`; the default suite only parses the
bundled ICO and records actions.

## Why the app does not call the keyring itself

One owner of the policy: which service name, which scope, what happens on failure and how the secret
is wiped. A second implementation would eventually diverge, and a store crate that reads the vault
could not be tested without touching the user's credentials.
