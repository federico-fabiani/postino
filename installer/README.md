# Postino Windows packaging (P12e)

Per-user Windows packaging for the frozen `x86_64-pc-windows-gnu` binaries. No administrator,
no Windows service, no autostart switch, no updater and no telemetry. The installer never
enables the existing opt-in autostart setting.

## Files

| Path | Purpose |
| --- | --- |
| `Build-Package.ps1` | Builds/stages the release payload, verifies runtime DLLs, writes `SHA256SUMS.txt` and the portable ZIP. |
| `Build-Msi.ps1` | Generates the WiX file list from the staged payload and builds the per-user MSI with WiX v4. |
| `Test-Package.ps1` | Static checks plus payload/manifest/provenance/DLL checks. Does not build an MSI or need a certificate. |
| `tests/Test-Resolvers.ps1` | Fixture tests for the build-stamp provenance gate, the toolchain-only DLL allowlist and PE architecture. |
| `wix/Postino.wxs` | Per-user MSI source: install folder, Start Menu shortcut, uninstall data choice. |
| `wix/license.rtf` | Licence shown by `WixUI_Minimal`. |
| `smoke-checklist.md` | Clean-VM acceptance checklist. |
| `lib/common.ps1` | Shared helpers. |

## Build the portable payload

From the repository root, on the `x86_64-pc-windows-gnu` host:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Package.ps1
```

The script builds only `postino-app`, `postino-cli` and `postino-daemon` with the shared
`target` directory. It cleans only its own `dist\Postino-<version>-windows-x64` folder. Use
`-DryRun` to print the plan, and `-Sign` only when an external certificate is installed (see below).

### Provenance and `-SkipBuild`

A build without `-SkipBuild` writes `target\postino-build-stamp.json` (or
`<CARGO_TARGET_DIR>\postino-build-stamp.json`) with the full 40-hex source commit, the clean/dirty
state and the SHA-256 of each release binary. `BUILD-INFO.txt` repeats the commit, the provenance
state and the same three hashes.

`-SkipBuild` is **refused** unless that stamp exists, names a commit that exists in this repository,
matches the hash of every release binary, and reports a clean tree. A pre-existing `target\release`
is therefore never labelled with `HEAD` on trust. If the tree is dirty, `Build-Package.ps1` warns
and stamps `Provenance: dirty`, and `Test-Package.ps1` fails the release check for that payload.

Output:

```text
dist/
  Postino-0.1.0-windows-x64/
    postino-app.exe
    postino.exe
    postino-daemon.exe
    <required MinGW runtime DLLs, if any>
    postino.ico
    LICENSE-MIT, LICENSE-APACHE, ATTRIBUTION.toml, README.md, SECURITY.md
    BUILD-INFO.txt
    SHA256SUMS.txt
  Postino-0.1.0-windows-x64.zip
  SHA256SUMS.txt              # hashes of the ZIP and, if built, the MSI
```

`SHA256SUMS.txt` uses the conventional `hash  relative-path` form and covers every staged
file except itself. Verify with PowerShell:

```powershell
Get-Content dist\SHA256SUMS.txt
certutil -hashfile dist\Postino-0.1.0-windows-x64.zip SHA256
```

## Install (MSI, per user, no admin)

Build the MSI only after staging:

```powershell
dotnet tool install --global wix
wix extension add -g WixToolset.UI.wixext
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Msi.ps1 `
  -StageDir dist\Postino-0.1.0-windows-x64
```

The MSI installs to `%LOCALAPPDATA%\Programs\Postino`, creates one Start Menu shortcut to
`postino-app.exe`, and registers a per-user uninstall entry. It requests no elevation and
installs no service. The CLI and daemon must stay next to the app because the CLI starts
`postino-daemon.exe` as a sibling:

```powershell
& "$env:LOCALAPPDATA\Programs\Postino\postino.exe" --version
& "$env:LOCALAPPDATA\Programs\Postino\postino.exe" daemon status
```

The installer does not add the install folder to `PATH`.

If WiX v4 is unavailable, `Build-Msi.ps1` exits with code 3, names the missing `wix.exe`, and
does not create an MSI. `-DryRun` prints the command line, requires a staged payload, and writes
nothing — not even `dist\obj\StageFiles.wxs`. `Test-Package.ps1` still validates the WiX source and
the generated file list. Never describe that state as a built MSI.

## Portable ZIP

Extract the ZIP anywhere writable and run `postino-app.exe` or `postino.exe`. There is no
installer, shortcut or uninstall entry. The portable ZIP is "no install", not "no local data":
without `--store` the app still uses `%LOCALAPPDATA%\Postino\mail.db`. For data that stays next
to the extracted folder, pass an explicit store path:

```powershell
.\postino-app.exe --store .\data\mail.db
.\postino.exe --store .\data\mail.db info
```

Do not run two writers on the same store. Keep the three executables and any staged DLLs in the
same folder.

## Uninstall and the data choice

Uninstall from **Settings > Apps > Installed apps > Postino > Uninstall**. It is a per-user
uninstall and does not prompt for elevation.

The uninstaller removes only what the MSI owns: the program folder, the Start Menu shortcut and
the uninstall entry. By default it keeps:

- `%LOCALAPPDATA%\Postino` — encrypted `mail.db`, attachments/blobs, `keyring.lock`;
- `%APPDATA%\Postino` — `daemon.json` and the bounded `daemon.log`.

On the uninstall path Postino shows one checkbox, **off by default**:

> Also delete my local mail, attachments, settings and logs

Ticking it and continuing removes those two Postino folders. Leaving it clear keeps them for a
reinstall. Credentials stored in Windows Credential Manager are not touched by the installer;
remove the Postino entries there yourself if you want to delete saved account passwords.

Close Postino and the daemon before uninstalling. If a file is still locked, the data deletion
is best-effort and the uninstall itself still completes; delete the two folders manually after
closing the process.

## Signing (optional, external certificate only)

The repository contains no certificate, private key, password or token. Signing uses a
certificate already installed in the Windows certificate store and `signtool.exe` from the
Windows SDK:

```powershell
$env:POSTINO_SIGN_CERT_SHA1 = "<40-hex thumbprint from the certificate store>"
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Package.ps1 -Sign
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Msi.ps1 `
  -StageDir dist\Postino-0.1.0-windows-x64 -Sign
```

`signtool` uses `/fd sha256`, `/tr <timestamp-url>` and `/td sha256` (RFC 3161). The timestamp
URL defaults to `http://timestamp.digicert.com`; pass `-TimestampUrl` to use another one. If
signing is requested without a valid thumbprint or `signtool`, the build fails instead of
producing a half-signed package. A signing failure after `wix build` removes the unsigned MSI from
`dist` and leaves no checksum entry for it, so an unsigned installer is never mistaken for a signed
one.

### Unsigned download warning

Unsigned builds are expected during development. Windows SmartScreen or Defender may show
"Windows protected your PC" or an unknown-publisher warning. Do not disable security controls.
Check the published `SHA256SUMS.txt` before running the file, or use a signed build. The
manifest is the integrity check; it is not a substitute for a trusted publisher signature.

## GNU runtime requirement

The binaries are `x86_64-pc-windows-gnu`, not MSVC. `Build-Package.ps1` reads each staged PE's
imports with `objdump` and copies every non-system DLL next to the executables (typically
`libgcc_s_seh-1.dll`, `libstdc++-6.dll` and `libwinpthread-1.dll`). A clean VM therefore does not
need MSYS2 or a separately installed MinGW runtime. The staging step fails loudly if an imported
DLL cannot be found; do not ship a payload that skipped that check.

DLL lookup is an explicit allowlist, never a search of `PATH`:

- the toolchain directory is `$env:POSTINO_MINGW_BIN`, else the directory that owns `objdump.exe`,
  else a known MSYS2 prefix (`C:\msys64\mingw64\bin` and friends);
- the resolved file must live inside that directory (full-path prefix check) and must be
  `x86_64` in its PE header; a 32-bit or foreign DLL is refused;
- `System32`/`SysWOW64` are matched by full Windows path only, either as an already-satisfied
  system import or as a refused source, and never staged.

If `objdump.exe` is missing, `Test-Package.ps1` reports an **INFO** skip for the import check
instead of a false failure; a release still needs that check to have run.

MSVC is not a target: this host lacks `link.exe` and Visual Studio Build Tools.

## No update service, telemetry or autostart

- No updater, update check, scheduled task or background download. Releases are manual,
  signed downloads; the checksum manifest is published with the build.
- No telemetry or analytics. The only network destinations are the user's own mail/calendar
  servers, configured by the user. MCP is a separate opt-in stdio process.
- The installer does not write `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` and does
  not start the app or daemon after install. Autostart remains the existing opt-in setting in
  Postino's own settings/CLI.
- No service and no machine-wide install. The MSI is `perUser` and writes no elevation request.

## Validation

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Test-Package.ps1 `
  -StageDir dist\Postino-0.1.0-windows-x64
```

It checks PowerShell syntax, the WiX contract (per-user scope, no admin/service/autostart,
default-off data deletion, shortcut target, generated file list determinism), the absence of
certificates/secrets/downloads in `installer/`, every payload hash, the provenance gate (a real
clean commit and the shipped binary hashes), the `x86_64` architecture of every staged PE, and that
every non-system DLL import resolves inside the payload. It also runs the fixture tests in
`tests/Test-Resolvers.ps1` and reports whether WiX is installed; it does not build or claim an MSI.

## Limits

- This package ships the desktop app, CLI and daemon. Tray/autostart remain product features,
  not installer features.
- ARM64 and MSVC packages are out of scope.

### Unverified on this host (do not claim otherwise)

- **WiX was never run.** No `wix.exe`/`dotnet` WiX toolchain is installed here, so the MSI source
  and the generated fragment have only been static-validated. The first real `wix build` may still
  report schema or extension errors.
- **No MSI exists and none was signed.** The staging layout, manifest and checks are verified; the
  installer itself is not.
- **No clean-VM run.** Install, upgrade, uninstall and the data-removal checkbox have not been
  executed on a fresh Windows VM; `smoke-checklist.md` is the manual acceptance list.
- The portable ZIP and its `SHA256SUMS.txt` are produced by the scripts but were not built in this
  review round, which had no release binaries to stage.
