# Security policy

Postino handles mail, credentials and attachments. Please do not open a public issue for a
suspected vulnerability.

- Contact the maintainer privately with a minimal reproduction, the affected version and the
  impact you believe it has.
- Allow reasonable time for a fix and a coordinated disclosure.
- The threat model, the invariants and the out-of-scope cases are in
  [docs/threat-model.md](docs/threat-model.md).

## Supported versions

Postino is pre-1.0. Only the current `main` branch and the latest prerelease receive security
fixes. There is no long-term support promise yet.

## What is in scope

- MIME/HTML parsing and sanitization.
- Attachment handling, path traversal, decompression and resource exhaustion.
- Credential storage, keyring use and at-rest encryption.
- TLS validation and network protocol handling.
- The local daemon IPC and MCP surface, including an agent sending on the user's behalf.
- The webview sandbox once it exists.

## What is not

- A compromised operating system, kernel or provider.
- Zero-days in the OS webview or TLS stack.
- "Unhackable" claims; see the threat model.
