# postino-ipc

Foundation transport for the Postino daemon. It owns three things and nothing else:

1. **The local endpoint.** A Windows named pipe (DACL: one generic-all ACE for the current user) or
   a Unix domain socket (mode 0600 inside a mode-0700 directory). There is no TCP listener and no
   remote variant in the type.
2. **The handshake.** The first frame is `hello {protocol, client, token, allow_send}`. The token is 32
   random bytes written by the daemon to `<store>.token` with an owner-only ACL/mode. A bad token is the
   same generic refusal as a missing one; a protocol mismatch is readable; the expected token is
   never echoed or logged. `allow_send` is the MCP session's send permission.
3. **The frame and queue bounds.** Newline-delimited JSON-RPC 2.0, at most 16 MiB per frame in each
   direction. A reader drains an oversized line without allocating it; an oversized response becomes a
   same-id `FRAME_TOO_LARGE` error and the connection stays open. Each connection has eight in-flight
   requests, a 64-value write queue, at most four subscriptions and a 64-event subscription window; the
   server caps total dispatches at 16, subscriptions at 64 and connections at 32. The 120 s idle timeout
   applies only when no request/subscription is live; a 15 s heartbeat and a 30 s write timeout cover
   long work and a stalled reader. A slow client loses events rather than growing the daemon.

The operation registry itself lives in `postino-core`: `op {name, args}` is the only way to run an
operation. This crate never keeps a second list of names or schemas.

## Server / client

```rust
let paths = DaemonPaths::for_store(store_path)?;
let token = SharedToken::generate()?;
TokenFile::new(paths.token_file.clone()).write(&token)?;
let server = Server::new(ServerConfig::new(paths.endpoint.clone(), token), dispatcher, hub);
server.run(shutdown).await?;
```

A client reads the token file, connects, sends `hello`, then `op`, `daemon_status`,
`events_subscribe`, `cancel` or `daemon_shutdown`. `BlockingClient` wraps the async client in a
current-thread runtime for the CLI and MCP server.

## Failure model

`IpcError` distinguishes absent daemon, bad token, version mismatch, busy mutation, cancellation,
overload and a typed daemon error. A client must show "daemon non in esecuzione" and the documented
fallback; it must never treat absence as a crash. The server never panics on a malformed frame: the
frame is drained, the error is written, and the connection is closed if it cannot be resynchronised.

## Tests

`cargo test -p postino-ipc` starts a real named pipe/socket for every server test. It covers token
accept/refuse, version mismatch, oversized framing, one in-flight mutation, cross-connection cancel,
events, unknown operations and the owner-only ACL. The 16 MiB frame test is deliberately a real
16 MiB line; it takes a few seconds on the Windows debug build.

## Limits and non-goals

- A 25 MiB attachment is about 35 MB of base64 and does not fit the 16 MiB frame. The CLI refuses it
  over IPC with a readable sentence; P10a2 may add an out-of-band blob channel.
- No TCP, no WebSocket, no remote peer, no TLS: the peer is the same user on the same machine.
- The token is not in argv, environment, logs or error text. `SharedToken`'s `Debug`/`Display` are
  redacted.
