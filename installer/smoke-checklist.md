# Postino clean-VM smoke checklist (P12e)

Run on a disposable Windows 10/11 x64 VM **without MSYS2 or Rust installed**. Use a snapshot you
can throw away. Record the package filename, SHA-256 and whether it was signed.

## 0. Artifact

- [ ] Copy `Postino-<version>-windows-x64.zip` (and `.msi` if one was built) and the published
      `SHA256SUMS.txt` to the VM.
- [ ] `certutil -hashfile <file> SHA256` matches `SHA256SUMS.txt` (or `Get-FileHash`, where available).
- [ ] `BUILD-INFO.txt` reports `Provenance: verified`, `Commit dirty: no` and a full commit hash that
      matches the release tag; its three `Binary ...:` hashes match the `SHA256SUMS.txt` lines.
- [ ] If signed: `Get-AuthenticodeSignature <file>` is `Valid` and the timestamp is present.
- [ ] If unsigned: the expected unknown-publisher/SmartScreen warning appears; no security
      setting was disabled to run it.

## 1. Portable ZIP

- [ ] Extract to `%USERPROFILE%\Desktop\Postino-portable`.
- [ ] No `.dll` error appears when starting `postino-app.exe` (GNU runtime is bundled).
- [ ] App opens; the first-run/onboarding screen is usable; `--demo` opens the synthetic inbox.
- [ ] `.\postino.exe --version` prints the package version.
- [ ] `.\postino.exe --demo info` returns a result; no crash.
- [ ] No installer entry, Start Menu shortcut or service exists after running the portable copy.
- [ ] Without `--store`, data is at `%LOCALAPPDATA%\Postino`; an explicit `--store` keeps it
      beside the extracted folder.

## 2. MSI install (per user, no admin)

- [ ] Run the MSI as a standard user. No UAC/elevation prompt appears.
- [ ] Install completes without a service or firewall prompt.
- [ ] Program files are in `%LOCALAPPDATA%\Programs\Postino`.
- [ ] One Start Menu shortcut exists under **Postino** and launches the app.
- [ ] `%LOCALAPPDATA%\Programs\Postino\postino.exe --version` works.
- [ ] `postino daemon status` reports a stopped/absent daemon; the installer did not start one.
- [ ] Settings > Apps lists Postino for the current user only.
- [ ] `Get-Service *postino*` finds nothing.
- [ ] `Get-ScheduledTask *postino*` finds nothing.
- [ ] `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` has no `Postino` value.
- [ ] No new listening port belongs to Postino (`Get-NetTCPConnection -State Listen`).
- [ ] While idle, no Postino process contacts an update/telemetry endpoint. Only a
      user-configured IMAP/SMTP/CalDAV server may be contacted; MCP stays opt-in on stdio.
- [ ] App can open the default store and, if a test account is configured, sync; then close the
      app and daemon.

## 3. Uninstall: keep data (default)

- [ ] Create disposable data with `--store` under `%TEMP%\postino-smoke\mail.db`, or use `--demo`.
- [ ] Uninstall from Settings > Apps. No elevation prompt.
- [ ] The uninstall dialog shows the data checkbox **unchecked**.
- [ ] Continue without ticking it.
- [ ] Program folder, Start Menu shortcut and uninstall entry are gone.
- [ ] `%LOCALAPPDATA%\Postino` and `%APPDATA%\Postino` still exist and still contain the data.

## 4. Uninstall: delete data (explicit choice)

- [ ] Reinstall, then create a disposable mail store and log.
- [ ] Uninstall again, tick **Also delete my local mail, attachments, settings and logs**, continue.
- [ ] Program files, shortcut, uninstall entry, `%LOCALAPPDATA%\Postino` and `%APPDATA%\Postino`
      are gone.
- [ ] Windows Credential Manager still lists the Postino entries (documented behaviour; delete
      them manually if desired).
- [ ] Uninstall completes even if a file was locked; manual cleanup is documented.

## 5. Reinstall and upgrade

- [ ] Reinstall over an existing per-user install without an admin prompt.
- [ ] Start Menu shortcut still works exactly once.
- [ ] A newer MSI with the same UpgradeCode removes the old version and keeps user data.
- [ ] `SHA256SUMS.txt` in the installed folder matches the staged files.
