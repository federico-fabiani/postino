# Gmail OAuth in Postino

Postino signs in to Gmail with OAuth 2.0 and SASL XOAUTH2 for both IMAP and SMTP. This page states
what the feature does and, more importantly, what the user has to provide and live with.

## Bring your own OAuth client

Postino ships no Google client secret. The user creates an OAuth client in their own Google Cloud
project:

- Application type **Desktop app** (not Web application). The Web-client flow with a client secret
  managed by Postino does not exist and is not supported.
- The loopback redirect is `http://127.0.0.1:<port>`: Postino binds an ephemeral port on the
  loopback interface, opens the system browser, validates `state` and exchanges the code with PKCE
  S256. No device code, no embedded webview, no other redirect.
- Scope: `https://mail.google.com/`.

The client id is public. The client secret and the refresh token are written to the OS keyring
(`account:<id>:oauth`) and nowhere else: not argv, not logs, not the encrypted store, not a result.
Google is contacted only for the authorization, token, refresh and revoke calls.

## Honest limits

- **Testing mode expires refresh tokens after 7 days.** A Google Cloud OAuth consent screen left in
  Testing grants refresh tokens that stop working after seven days, so the user has to authorize
  again. Publishing the app avoids that; it is a user/Google decision, not something Postino can
  fix.
- **Verification and CASA cost time and money.** An unverified app shows the "Google hasn't verified
  this app" warning and has a capped number of users. Verification for restricted scopes can require
  a security assessment (CASA); Postino does not pay for or automate that.
- **Gmail only.** Other providers use an app password; Aruba has no OAuth seam in this build. There
  is no silent fallback: an OAuth account never sends a password, and an app-password account never
  sends XOAUTH2.
- **Revocation is best effort after local removal.** Removing an account deletes it locally even when
  Google cannot be reached. The operation then returns a warning: the grant stays valid until it
  expires or is revoked at `myaccount.google.com` → Security → Third-party access.
- **The app password remains a first-class option.** 2-Step Verification plus an app password needs
  no Google Cloud project and does not expire every seven days.

## Related

- [Architecture](architecture.md) for the net/core/platform layering.
- [Threat model](threat-model.md) for the keyring and local-store boundaries.
