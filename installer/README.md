# Postino Windows packaging (P12e)

Per-user Windows packaging for the frozen x86_64 Windows binaries: the GNU host locally, the MSVC
host with a static CRT for releases. No administrator, no Windows service, no autostart switch, no
background updater and no telemetry. The installer never enables the existing opt-in autostart
setting. The one update path is explicit: a check at start and a button the person presses, which
verifies the published MSI and hands it to Windows Installer.

## Files

| Path | Purpose |
| --- | --- |
| `Build-Package.ps1` | Builds/stages the release payload, verifies runtime DLLs, writes `SHA256SUMS.txt` and the portable ZIP. |
| `Build-Msi.ps1` | Generates the WiX file list from the staged payload and builds the per-user MSI with WiX v4. |
| `Test-Package.ps1` | Static checks plus payload/manifest/provenance/DLL checks. Does not build an MSI or need a certificate. |
| `tests/Test-Resolvers.ps1` | Fixture tests for the build-stamp provenance gate, the toolchain-only DLL allowlist and PE architecture. |
| `wix/Postino.wxs` | Per-user MSI source: install folder, Start Menu shortcut, uninstall data choice. |
| `wix/installer-ui.wxs` | Four text-only native dialogs: welcome, maintenance, progress, outcome. No WiX UI extension, no licence page. |
| `smoke-checklist.md` | Clean-VM acceptance checklist. |
| `lib/common.ps1` | Shared helpers. |

## Build the portable payload

From the repository root, on the `x86_64-pc-windows-gnu` host:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Package.ps1
```

On the release runner the same script runs with the MSVC host and the static CRT
(`.cargo/config.toml`), which stages no runtime DLL at all:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Package.ps1 -Target x86_64-pc-windows-msvc
```

The script builds only `postino-app`, `postino-cli` and `postino-daemon` with the shared
`target` directory. It cleans only its own `dist\Postino-<version>-windows-x64` folder. Use
`-DryRun` to print the plan, and `-Sign` only when an external certificate is installed (see below).
The target must match the rustc host: GNU here, MSVC on the release runner.

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
  Postino-0.1.1-windows-x64/
    postino-app.exe
    postino.exe
    postino-daemon.exe
    <required MinGW runtime DLLs, if any>
    postino.ico
    LICENSE-MIT, LICENSE-APACHE, ATTRIBUTION.toml, README.md, SECURITY.md
    BUILD-INFO.txt
    SHA256SUMS.txt
  Postino-0.1.1-windows-x64.zip
  SHA256SUMS.txt              # hashes of the ZIP and, if built, the MSI
```

`SHA256SUMS.txt` uses the conventional `hash  relative-path` form and covers every staged
file except itself. Verify with PowerShell:

```powershell
Get-Content dist\SHA256SUMS.txt
certutil -hashfile dist\Postino-0.1.1-windows-x64.zip SHA256
```

## Install (MSI, per user, no admin)

Build the MSI only after staging:

```powershell
dotnet tool install --global wix
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Build-Msi.ps1 `
  -StageDir dist\Postino-0.1.1-windows-x64
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

`wix/installer-ui.wxs` replaces the stock WiX wizard. Postino shows four native Windows Installer
dialogs — a welcome page with Install, a maintenance page with Repair/Remove and the "remove my
data" checkbox, a progress page with a working Cancel, and a single outcome page for success,
failure or cancellation — plus the stock "files in use" prompt. There is no licence page, no
`WixUI_Minimal`, no `WixToolset.UI.wixext` and no `license.rtf`: the licence is in `LICENSE-MIT`
and `LICENSE-APACHE`, and `Test-Package.ps1` fails if the hand-written UI is missing or if a stock
UI reference comes back.

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
  -StageDir dist\Postino-0.1.1-windows-x64 -Sign
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

## Runtime DLLs: GNU payload, MSVC release

Local builds are `x86_64-pc-windows-gnu`. For them `Build-Package.ps1` reads each staged PE's
imports with `objdump` and copies every non-system DLL next to the executables (typically
`libgcc_s_seh-1.dll`, `libstdc++-6.dll` and `libwinpthread-1.dll`). A clean VM therefore does not
need MSYS2 or a separately installed MinGW runtime. The staging step fails loudly if an imported
DLL cannot be found; do not ship a payload that skipped that check.

The release build is `x86_64-pc-windows-msvc` with `+crt-static` (`.cargo/config.toml`). Its
executables import only Windows system DLLs, so the staging step copies no DLL and needs no
`objdump`, `dlltool` or MinGW toolchain. That is the payload CI ships; it is also smaller.

DLL lookup, when the GNU target is built, is an explicit allowlist, never a search of `PATH`:

- the toolchain directory is `$env:POSTINO_MINGW_BIN`, else the directory that owns `objdump.exe`,
  else a known MSYS2 prefix (`C:\msys64\mingw64\bin` and friends);
- the resolved file must live inside that directory (full-path prefix check) and must be
  `x86_64` in its PE header; a 32-bit or foreign DLL is refused;
- `System32`/`SysWOW64` are matched by full Windows path only, either as an already-satisfied
  system import or as a refused source, and never staged.

If `objdump.exe` is missing, `Test-Package.ps1` reports an **INFO** skip for the import check
instead of a false failure; a release still needs that check to have run.

An MSVC payload legitimately imports no non-system DLL, so the import check passes with an empty
set there.

## Update check, telemetry and autostart

- **One update check, on demand.** The `update_check` operation asks
  `https://api.github.com/repos/federico-fabiani/postino/releases/latest` for the newest published
  release: one request, a 10 s timeout, a 1 MiB answer cap, no download and no install. The desktop
  app asks once at start and the banner can be dismissed for the session; the CLI and MCP ask only
  when called. The answer is a version tag and a release page, and only a page under the project is
  accepted.
- **One explicit install.** When the banner offers a newer release, the button runs `update_install`:
  it downloads the `-windows-x64.msi` of that release, refuses it unless its SHA-256 matches the
  `SHA256SUMS.txt` of the same release, writes it under `%LOCALAPPDATA%\Postino\updates` and starts
  `msiexec /qb` detached. Nothing is downloaded or installed without that press.
- No background download, no scheduled task and no silent install. An update is always an
  installer the user chooses to run.
- No telemetry or analytics. The only other network destinations are the user's own mail/calendar
  servers, configured by the user. MCP is a separate opt-in stdio process.
- The installer does not write `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` and does
  not start the app or daemon after install. Autostart remains the existing opt-in setting in
  Postino's own settings/CLI.
- No service and no machine-wide install. The MSI is `perUser` and writes no elevation request.

## Validation

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File installer\Test-Package.ps1 `
  -StageDir dist\Postino-0.1.1-windows-x64
```

It checks PowerShell syntax, the WiX contract (per-user scope, no admin/service/autostart,
default-off data deletion, shortcut target, generated file list determinism), the absence of
certificates/secrets/downloads in `installer/`, every payload hash, the provenance gate (a real
clean commit and the shipped binary hashes), the `x86_64` architecture of every staged PE, and that
every non-system DLL import resolves inside the payload. It also runs the fixture tests in
`tests/Test-Resolvers.ps1`, reports whether WiX is installed, and when an MSI exists checks that its
hash matches `dist\SHA256SUMS.txt`; it does not build the installer or sign anything.

## Limits

- This package ships the desktop app, CLI and daemon. Tray/autostart remain product features,
  not installer features.
- ARM64 packages are out of scope.

### Unverified on this host (do not claim otherwise)

- **The MSI is unsigned.** WiX v4.0.6 built `dist\Postino-0.1.1-windows-x64.msi` from the staged
  payload; no certificate is installed, so the binary carries no Authenticode signature.
- **No clean-VM run.** The MSI was installed and uninstalled silently on the Windows 11 development
  host (both exit 0; the staged `postino.exe --version` ran; install folder and Start Menu shortcut
  were removed). A fresh Windows VM, an upgrade over an older build, and the data-removal checkbox
  remain manual checks; `smoke-checklist.md` is the acceptance list.
- The portable ZIP and its `SHA256SUMS.txt` were built from the release binaries in this round; the
  MSI hash is included in `dist\SHA256SUMS.txt`.
- **Upgrade with a running daemon is untested.** `update_install` closes the app before `msiexec`
  starts, but a running `postino-daemon.exe` still owns its own binary, so Windows Installer may ask
  to close it ("files in use"). Stopping the daemon first is the next improvement.
