use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::ServerName;
use tokio::net::{TcpStream, lookup_host};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

use crate::NetError;
use crate::capabilities::Capabilities;
use crate::config::{ImapConfig, TlsMode};
use crate::error::{protocol, sanitize_reason};
use crate::fetch::FetchData;
use crate::folder::{MailboxStatus, RemoteFolder};
use crate::mailbox::astring;
use crate::message::{Envelope, Flag, FlagMode, IdleEvent};
use crate::response::{Response, Status, StatusCode, Untagged, parse_response};
use crate::secret::Secret;
use crate::tls;
use crate::uid::UidSet;
use crate::wire::{Budget, Connection, Stream};

/// Longest idle wait this client will request: below the 30-minute limit RFC 2177 sets for servers.
const MAX_IDLE_WAIT: Duration = Duration::from_secs(29 * 60);
/// Longest flag list accepted by [`Session::store_flags`].
const MAX_STORED_FLAGS: usize = 64;

/// The result of one command: untagged responses plus the code of the tagged `OK`.
struct Outcome {
    untagged: Vec<Untagged>,
    code: Option<StatusCode>,
}

/// One authenticated IMAP session.
///
/// Commands run one at a time. The session keeps the tag counter, the capabilities, the selected
/// mailbox and the password only for as long as they are needed; after a successful login the
/// password is dropped (and zeroised).
pub struct Session {
    conn: Connection,
    tag: u32,
    capabilities: Capabilities,
    authenticated: bool,
    selected: Option<String>,
    config: ImapConfig,
    closed: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("tag", &self.tag)
            .field("authenticated", &self.authenticated)
            .field("selected", &self.selected)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open a connection, verify TLS, read the greeting and ask for `CAPABILITY`.
    ///
    /// With [`TlsMode::StartTls`] the exchange before the upgrade is the greeting and
    /// `CAPABILITY` only: no credential crosses the socket in plaintext, and the client refuses a
    /// server that does not advertise `STARTTLS`.
    pub async fn connect(config: ImapConfig) -> Result<Self, NetError> {
        if config.host.trim().is_empty() {
            return Err(NetError::Connect { host: config.host, port: config.port, reason: "host is empty".to_owned() });
        }
        if config.timeout.is_zero() {
            return Err(NetError::Timeout { phase: "connect", after: config.timeout });
        }

        let mut config = config;
        config.limits = config.limits.clamped();
        let limits = config.limits;
        let timeout = config.timeout;

        let addresses = resolve(&config.host, config.port, timeout).await?;
        let tcp = connect_any(&config.host, config.port, &addresses, timeout).await?;
        if let Err(error) = tcp.set_nodelay(true) {
            log::debug!("imap: could not disable Nagle: {error}");
        }

        let conn = Connection::new(Stream::Plain(tcp), limits, timeout);
        let mut session = Self { conn, tag: 0, capabilities: Capabilities::default(), authenticated: false, selected: None, config, closed: false };

        if session.config.tls == TlsMode::Implicit {
            session.handshake().await?;
        }
        session.greet().await?;
        if session.config.tls == TlsMode::StartTls {
            session.starttls().await?;
        } else {
            session.refresh_capabilities().await?;
        }
        Ok(session)
    }

    /// What the server advertised on the last `CAPABILITY`.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// The mailbox currently selected, as it was passed to [`Session::select`].
    #[must_use]
    pub fn selected_mailbox(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Authenticate: `AUTHENTICATE PLAIN` when the server offers it, `LOGIN` otherwise.
    ///
    /// A server that advertises `LOGINDISABLED` is refused with [`NetError::Auth`] instead of a
    /// retry. A `PREAUTH` greeting makes this a no-op. When the tagged `OK` carries
    /// `[CAPABILITY ...]` that list replaces the current one; otherwise the pre-login list stands,
    /// because a server that does not volunteer the new list has not claimed anything changed.
    pub async fn login(&mut self) -> Result<(), NetError> {
        if self.closed {
            return Err(NetError::Closed { phase: "auth" });
        }
        if self.authenticated {
            return Ok(());
        }
        if self.capabilities.login_disabled() {
            return Err(NetError::Auth {
                reason: "the server advertises LOGINDISABLED and will not accept credentials on this connection".to_owned(),
            });
        }
        // TLS is guaranteed here: `connect` either completed the handshake (implicit) or refused to
        // leave `starttls` without an upgraded stream.
        if self.capabilities.auth_plain() { self.authenticate_plain().await } else { self.login_with_password().await }
    }

    /// `LIST "" "*"`, with the folder count bounded.
    pub async fn list(&mut self) -> Result<Vec<RemoteFolder>, NetError> {
        let outcome = self.execute("list", "LIST", Some("\"\" \"*\"")).await?;
        let limit = self.config.limits.max_folders;
        let mut folders = Vec::new();
        for item in outcome.untagged {
            if let Untagged::List(folder) = item {
                folders.push(folder);
                if folders.len() > limit {
                    return Err(NetError::TooLarge { what: "folders", limit });
                }
            }
        }
        Ok(folders)
    }

    /// `EXAMINE`: open a mailbox read-only.
    pub async fn examine(&mut self, mailbox: &str) -> Result<MailboxStatus, NetError> {
        self.select_inner("examine", "EXAMINE", mailbox).await
    }

    /// `SELECT`: open a mailbox read-write.
    pub async fn select(&mut self, mailbox: &str) -> Result<MailboxStatus, NetError> {
        self.select_inner("select", "SELECT", mailbox).await
    }

    /// `UID FETCH` of UID, flags, `INTERNALDATE`, size and `ENVELOPE` for each UID in the set.
    ///
    /// A response without an envelope (an unsolicited flag change from another client) is skipped.
    /// A returned envelope whose UID is outside the requested set is refused: it is the wrong
    /// message, and storing it would corrupt the folder.
    pub async fn fetch_envelopes(&mut self, uids: UidSet) -> Result<Vec<Envelope>, NetError> {
        let set = uids.to_wire()?;
        let arguments = format!("{set} (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)");
        let outcome = self.execute("fetch", "UID FETCH", Some(&arguments)).await?;
        let mut envelopes = Vec::new();
        for item in outcome.untagged {
            let Untagged::Fetch(data) = item else {
                continue;
            };
            let FetchData { uid: Some(uid), envelope: Some(envelope), flags, internal_date, size, .. } = *data else {
                continue;
            };
            if !uids.contains(uid) {
                return Err(protocol(format!("the server returned UID {uid}, outside the requested set")));
            }
            envelopes.push(Envelope {
                uid,
                flags,
                internal_date,
                size: size.unwrap_or(0),
                subject: envelope.subject.unwrap_or_default(),
                from: envelope.from,
                to: envelope.to,
                cc: envelope.cc,
                bcc: envelope.bcc,
                reply_to: envelope.reply_to,
                message_id: envelope.message_id,
                in_reply_to: envelope.in_reply_to,
                references: Vec::new(),
                list_id: None,
            });
        }
        Ok(envelopes)
    }

    /// `UID FETCH <uid> (BODY.PEEK[])`: the whole message, bounded by [`crate::Limits::max_literal`].
    ///
    /// `BODY.PEEK` is not decoration: it stops the server from setting `\Seen`, so reading a message
    /// never changes its state behind the user's back. A body that arrives with another UID is
    /// refused instead of being handed back as the message the caller asked for.
    pub async fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, NetError> {
        if uid == 0 {
            return Err(protocol("UID 0 does not exist"));
        }
        let arguments = format!("{uid} (BODY.PEEK[])");
        let outcome = self.execute("fetch body", "UID FETCH", Some(&arguments)).await?;
        for item in outcome.untagged {
            let Untagged::Fetch(data) = item else {
                continue;
            };
            let FetchData { body: Some(body), uid: returned, .. } = *data else {
                continue;
            };
            if returned != Some(uid) {
                return Err(protocol(match returned {
                    Some(other) => format!("the server returned UID {other} for a fetch of UID {uid}"),
                    None => format!("the server returned a body without a UID for a fetch of UID {uid}"),
                }));
            }
            return Ok(body);
        }
        Err(protocol("the server returned no BODY[] for the message"))
    }

    /// `UID STORE`, always silent and always with `UID`.
    ///
    /// [`FlagMode::Add`] writes `+FLAGS.SILENT`, [`FlagMode::Remove`] `-FLAGS.SILENT`, and
    /// [`FlagMode::Replace`] `FLAGS.SILENT`; the new flags are not needed locally because the sync
    /// layer already knows the intent.
    pub async fn store_flags(&mut self, uids: UidSet, flags: &[Flag], mode: FlagMode) -> Result<(), NetError> {
        check_storable_flags(flags)?;
        let set = uids.to_wire()?;
        let names = flags.iter().map(Flag::as_wire).collect::<Vec<_>>().join(" ");
        let arguments = format!("{set} {} ({names})", mode.as_wire());
        let _ = self.execute("store", "UID STORE", Some(&arguments)).await?;
        Ok(())
    }

    /// `NOOP`: a cheap round trip that also lets the server flush pending untagged responses.
    pub async fn noop(&mut self) -> Result<(), NetError> {
        let _ = self.execute("noop", "NOOP", None).await?;
        Ok(())
    }

    /// One `IDLE` cycle: `IDLE`, wait for the continuation, collect events for at most `wait`,
    /// send `DONE`, require the tagged `OK`.
    ///
    /// The wait is clamped to 29 minutes, below the server's 30-minute limit; [`Duration::ZERO`]
    /// sends `DONE` immediately. A mailbox must be selected. A server `BYE` is returned as
    /// [`IdleEvent::Bye`] and closes the session; a silent disconnect is [`NetError::Closed`].
    ///
    /// Only a quiet expiry of the wait ends the cycle normally. A timeout while a line or a literal
    /// is half-read, a malformed response, a bound error and a timeout waiting for the `+`
    /// continuation all close the session, because the stream is no longer at a command boundary.
    pub async fn idle(&mut self, wait: Duration) -> Result<Vec<IdleEvent>, NetError> {
        if self.closed {
            return Err(NetError::Closed { phase: "idle" });
        }
        // Local misuse is not a protocol failure: refuse before touching the wire, and leave the
        // session usable.
        if self.selected.is_none() {
            return Err(protocol("IDLE requires a selected mailbox"));
        }
        if !self.capabilities.idle() {
            return Err(NetError::Unsupported { feature: "IDLE" });
        }
        let result = self.idle_inner(wait).await;
        if let Err(error) = &result {
            self.note_failure(error);
        }
        result
    }

    async fn idle_inner(&mut self, wait: Duration) -> Result<Vec<IdleEvent>, NetError> {
        let tag = self.next_tag();
        let command = Zeroizing::new(format!("{tag} IDLE\r\n"));
        log::debug!("imap: {tag} IDLE");
        let deadline = self.deadline();
        self.conn.write_all(command.as_bytes(), "idle", deadline).await?;

        let mut budget = self.budget();
        let limit = self.config.limits.max_untagged;
        // Waiting for the continuation is a command exchange, not the idle window: a timeout here
        // means the server never entered IDLE, and the session must close.
        let mut pending = 0usize;
        loop {
            let segments = self.conn.read_response("idle", deadline, &mut budget).await?;
            match parse_response(&segments)? {
                Response::Continuation(_) => break,
                Response::Untagged(item) => {
                    pending += 1;
                    if pending > limit {
                        return Err(NetError::TooLarge { what: "untagged responses", limit });
                    }
                    if is_bye(&item) {
                        self.closed = true;
                        return Err(NetError::Closed { phase: "idle" });
                    }
                }
                Response::Tagged { tag: got, .. } => {
                    self.check_tag(&got, &tag)?;
                    return Err(protocol("IDLE was answered without a continuation"));
                }
            }
        }

        let mut events: Vec<IdleEvent> = Vec::new();
        let idle_deadline = Instant::now().checked_add(wait.min(MAX_IDLE_WAIT)).unwrap_or_else(Instant::now);
        loop {
            if Instant::now() >= idle_deadline {
                break;
            }
            match self.conn.read_response("idle", idle_deadline, &mut budget).await {
                Ok(segments) => match parse_response(&segments)? {
                    Response::Untagged(item) => {
                        let event = idle_event(item);
                        let bye = matches!(event, IdleEvent::Bye(_));
                        events.push(event);
                        if bye {
                            self.closed = true;
                            return Ok(events);
                        }
                        if events.len() > limit {
                            return Err(NetError::TooLarge { what: "idle events", limit });
                        }
                    }
                    Response::Continuation(text) => return Err(protocol(format!("unexpected continuation during IDLE: {text}"))),
                    Response::Tagged { tag: got, response } => {
                        // Some servers end IDLE themselves. Accept it and do not send DONE.
                        self.check_tag(&got, &tag)?;
                        return match response.status {
                            Status::Ok => Ok(events),
                            _ => Err(response.into_error("IDLE")),
                        };
                    }
                },
                // The wait is over only when the socket is quiet. A half line or a half literal
                // means the connection stalled in the middle of a response.
                Err(NetError::Timeout { .. }) if !self.conn.has_partial_input() => break,
                Err(error) => return Err(error),
            }
        }

        let done_deadline = self.deadline();
        self.conn.write_all(b"DONE\r\n", "idle", done_deadline).await?;
        loop {
            let segments = self.conn.read_response("idle", done_deadline, &mut budget).await?;
            match parse_response(&segments)? {
                Response::Untagged(item) => {
                    if is_bye(&item) {
                        self.closed = true;
                        return Err(NetError::Closed { phase: "idle" });
                    }
                    events.push(idle_event(item));
                    if events.len() > limit {
                        return Err(NetError::TooLarge { what: "idle events", limit });
                    }
                }
                Response::Continuation(text) => return Err(protocol(format!("unexpected continuation after DONE: {text}"))),
                Response::Tagged { tag: got, response } => {
                    self.check_tag(&got, &tag)?;
                    return match response.status {
                        Status::Ok => Ok(events),
                        _ => Err(response.into_error("IDLE")),
                    };
                }
            }
        }
    }

    /// Best-effort `LOGOUT`, then close. A broken connection on the way out is not an error.
    pub async fn logout(mut self) -> Result<(), NetError> {
        if !self.closed {
            let _ = self.execute("logout", "LOGOUT", None).await;
        }
        self.closed = true;
        Ok(())
    }

    async fn select_inner(&mut self, phase: &'static str, verb: &'static str, mailbox: &str) -> Result<MailboxStatus, NetError> {
        let name = astring(mailbox, "mailbox")?;
        self.selected = None;
        let outcome = self.execute(phase, verb, Some(&name)).await?;

        let mut status = MailboxStatus::default();
        let mut uid_validity = None;
        for item in outcome.untagged {
            match item {
                Untagged::Exists(count) => status.exists = count,
                Untagged::Recent(count) => status.recent = count,
                Untagged::Status(response) => apply_status_code(&mut status, &mut uid_validity, response.code.as_ref()),
                _ => {}
            }
        }
        // `A0003 OK [READ-ONLY] ...` is the normal RFC 3501 way to report a read-only mailbox.
        apply_status_code(&mut status, &mut uid_validity, outcome.code.as_ref());
        let uid_validity = uid_validity.ok_or_else(|| protocol("the SELECT response has no UIDVALIDITY"))?;
        status.uid_validity = uid_validity;
        if verb == "EXAMINE" {
            status.read_only = true;
        }
        self.selected = Some(mailbox.to_owned());
        Ok(status)
    }

    async fn authenticate_plain(&mut self) -> Result<(), NetError> {
        let result = self.authenticate_plain_inner().await;
        if let Err(error) = &result {
            self.note_failure(error);
        }
        result
    }

    /// The SASL exchange, with the same failure accounting the tagged commands get through
    /// [`Session::execute`]: a timeout, a close, a bound error or a malformed response closes the
    /// session, so a retry cannot send a second `AUTHENTICATE` down a desynchronised stream.
    async fn authenticate_plain_inner(&mut self) -> Result<(), NetError> {
        let phase = "auth";
        let tag = self.next_tag();
        let command = Zeroizing::new(format!("{tag} AUTHENTICATE PLAIN\r\n"));
        log::debug!("imap: {tag} AUTHENTICATE PLAIN");
        let deadline = self.deadline();
        self.conn.write_all(command.as_bytes(), phase, deadline).await?;

        let mut budget = self.budget();
        let limit = self.config.limits.max_untagged;
        let mut count = 0usize;
        loop {
            let segments = self.conn.read_response(phase, deadline, &mut budget).await?;
            match parse_response(&segments)? {
                Response::Continuation(_) => break,
                Response::Untagged(item) => {
                    count += 1;
                    if count > limit {
                        return Err(NetError::TooLarge { what: "untagged responses", limit });
                    }
                    if matches!(&item, Untagged::Status(status) if status.status == Status::Bye) {
                        self.closed = true;
                        return Err(NetError::Closed { phase });
                    }
                }
                Response::Tagged { tag: got, response } => {
                    self.check_tag(&got, &tag)?;
                    return Err(auth_error(response.into_error("AUTHENTICATE")));
                }
            }
        }

        let payload = sasl_plain(&self.config.username, self.config.password.expose());
        let line = Zeroizing::new(format!("{}\r\n", payload.as_str()));
        self.conn.write_all(line.as_bytes(), phase, deadline).await?;

        loop {
            let segments = self.conn.read_response(phase, deadline, &mut budget).await?;
            match parse_response(&segments)? {
                Response::Continuation(text) => return Err(protocol(format!("unexpected continuation: {text}"))),
                Response::Untagged(item) => {
                    count += 1;
                    if count > limit {
                        return Err(NetError::TooLarge { what: "untagged responses", limit });
                    }
                    if matches!(&item, Untagged::Status(status) if status.status == Status::Bye) {
                        self.closed = true;
                        return Err(NetError::Closed { phase });
                    }
                }
                Response::Tagged { tag: got, response } => {
                    self.check_tag(&got, &tag)?;
                    return match response.status {
                        Status::Ok => {
                            self.absorb_capabilities(response.code.as_ref());
                            self.authenticated = true;
                            self.forget_password();
                            Ok(())
                        }
                        _ => Err(auth_error(response.into_error("AUTHENTICATE"))),
                    };
                }
            }
        }
    }

    async fn login_with_password(&mut self) -> Result<(), NetError> {
        let user = astring(&self.config.username, "username")?;
        let password = astring(self.config.password.expose(), "password")?;
        let arguments = Zeroizing::new(format!("{user} {}", password.as_str()));
        let outcome = match self.execute("login", "LOGIN", Some(&arguments)).await {
            Ok(outcome) => outcome,
            Err(error) => return Err(auth_error(error)),
        };
        self.absorb_capabilities(outcome.code.as_ref());
        self.authenticated = true;
        self.forget_password();
        Ok(())
    }

    /// Drop and zeroise the password once it cannot be needed again.
    fn forget_password(&mut self) {
        self.config.password = Secret::new(String::new());
    }

    async fn greet(&mut self) -> Result<(), NetError> {
        let deadline = self.deadline();
        let mut budget = self.budget();
        let segments = self.conn.read_response("greeting", deadline, &mut budget).await?;
        match parse_response(&segments)? {
            Response::Untagged(Untagged::Status(status)) => match status.status {
                Status::Ok => {
                    self.absorb_capabilities(status.code.as_ref());
                    Ok(())
                }
                Status::PreAuth => {
                    self.absorb_capabilities(status.code.as_ref());
                    self.authenticated = true;
                    Ok(())
                }
                Status::Bye => {
                    self.closed = true;
                    Err(NetError::Closed { phase: "greeting" })
                }
                _ => Err(protocol("unexpected greeting")),
            },
            _ => Err(protocol("the server did not send an IMAP greeting")),
        }
    }

    async fn refresh_capabilities(&mut self) -> Result<(), NetError> {
        let outcome = self.execute("capability", "CAPABILITY", None).await?;
        let mut found = None;
        for item in outcome.untagged {
            if let Untagged::Capability(items) = item {
                found = Some(items);
            }
        }
        let items = found.ok_or_else(|| protocol("the server did not answer CAPABILITY"))?;
        self.capabilities = Capabilities::from_items(items);
        if !self.capabilities.imap4rev1() {
            return Err(NetError::Unsupported { feature: "IMAP4rev1" });
        }
        Ok(())
    }

    async fn starttls(&mut self) -> Result<(), NetError> {
        self.refresh_capabilities().await?;
        if !self.capabilities.starttls() {
            return Err(NetError::Unsupported { feature: "STARTTLS" });
        }
        let _ = self.execute("starttls", "STARTTLS", None).await?;
        if self.conn.has_buffered() {
            self.closed = true;
            return Err(protocol("the server sent plaintext after the STARTTLS OK"));
        }
        self.handshake().await?;
        // Everything the server said before the upgrade is untrusted: ask again over TLS.
        self.capabilities = Capabilities::default();
        self.authenticated = false;
        self.refresh_capabilities().await?;
        Ok(())
    }

    async fn handshake(&mut self) -> Result<(), NetError> {
        let config = tls::client_config(&self.config.extra_roots, &self.config.host)?;
        let connector = TlsConnector::from(config);
        let name = ServerName::try_from(self.config.host.clone())
            .map_err(|_| NetError::Tls { host: self.config.host.clone(), reason: "host is not a valid DNS name or IP address".to_owned() })?;
        let deadline = self.deadline();
        let stream = self.conn.take_stream();
        match timeout_at(deadline, connector.connect(name, stream)).await {
            Err(_) => {
                self.closed = true;
                Err(NetError::Timeout { phase: "tls", after: self.config.timeout })
            }
            Ok(Err(error)) => {
                self.closed = true;
                // A connection torn down mid-handshake is a closed connection, not a bad
                // certificate; keep the two distinguishable for the caller.
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                ) {
                    return Err(NetError::Closed { phase: "tls" });
                }
                Err(NetError::Tls { host: self.config.host.clone(), reason: sanitize_reason(&error.to_string()) })
            }
            Ok(Ok(stream)) => {
                self.conn.set_stream(Stream::Tls(Box::new(stream)));
                self.conn.discard_buffered();
                Ok(())
            }
        }
    }

    async fn execute(&mut self, phase: &'static str, verb: &'static str, arguments: Option<&str>) -> Result<Outcome, NetError> {
        if self.closed {
            return Err(NetError::Closed { phase });
        }
        let tag = self.next_tag();
        let command = Zeroizing::new(build_command(&tag, verb, arguments, self.config.limits.max_command)?);
        log::debug!("imap: {tag} {verb}");
        let deadline = self.deadline();
        if let Err(error) = self.conn.write_all(command.as_bytes(), phase, deadline).await {
            self.note_failure(&error);
            return Err(error);
        }
        match self.collect(phase, verb, &tag, deadline).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.note_failure(&error);
                Err(error)
            }
        }
    }

    async fn collect(&mut self, phase: &'static str, verb: &'static str, tag: &str, deadline: Instant) -> Result<Outcome, NetError> {
        let mut budget = self.budget();
        let limit = self.config.limits.max_untagged;
        let mut untagged = Vec::new();
        loop {
            let segments = self.conn.read_response(phase, deadline, &mut budget).await?;
            match parse_response(&segments)? {
                Response::Untagged(item) => {
                    if matches!(&item, Untagged::Status(status) if status.status == Status::Bye) {
                        self.closed = true;
                        return Err(NetError::Closed { phase });
                    }
                    untagged.push(item);
                    if untagged.len() > limit {
                        return Err(NetError::TooLarge { what: "untagged responses", limit });
                    }
                }
                Response::Continuation(text) => return Err(protocol(format!("unexpected continuation: {text}"))),
                Response::Tagged { tag: got, response } => {
                    self.check_tag(&got, tag)?;
                    return match response.status {
                        Status::Ok => Ok(Outcome { untagged, code: response.code }),
                        Status::No | Status::Bad => Err(response.into_error(verb)),
                        _ => Err(protocol("unexpected status in a tagged response")),
                    };
                }
            }
        }
    }

    fn check_tag(&mut self, got: &str, expected: &str) -> Result<(), NetError> {
        if got != expected {
            self.closed = true;
            return Err(protocol(format!("unexpected tag {got:?}, expected {expected:?}")));
        }
        Ok(())
    }

    fn absorb_capabilities(&mut self, code: Option<&StatusCode>) {
        if let Some(StatusCode::Capability(items)) = code {
            self.capabilities = Capabilities::from_items(items.iter().cloned());
        }
    }

    /// A failure that leaves the stream at an unknown position makes the session unusable; say so
    /// immediately next time, and drop the socket so the server does not keep a dead connection or a
    /// stuck `IDLE` alive.
    ///
    /// `Timeout` and `Closed` lose the socket, `Tls` never established one, and `Protocol` and
    /// `TooLarge` mean the client stopped mid-response (an unread literal, an over-long line, a
    /// number it refused): the next read would start in the middle of someone else's response. A
    /// `ServerNo`/`ServerBad` reply is complete, so those do not close the session.
    fn note_failure(&mut self, error: &NetError) {
        if matches!(
            error,
            NetError::Timeout { .. } | NetError::Closed { .. } | NetError::Tls { .. } | NetError::Protocol { .. } | NetError::TooLarge { .. }
        ) {
            self.closed = true;
            self.conn.abandon();
        }
    }

    fn next_tag(&mut self) -> String {
        self.tag = self.tag.saturating_add(1);
        format!("A{:04}", self.tag)
    }

    fn deadline(&self) -> Instant {
        Instant::now().checked_add(self.config.timeout).unwrap_or_else(Instant::now)
    }

    fn budget(&self) -> Budget {
        Budget::new(self.config.limits.max_response_bytes)
    }
}

/// Copy one response code into the mailbox status being assembled.
fn apply_status_code(status: &mut MailboxStatus, uid_validity: &mut Option<u32>, code: Option<&StatusCode>) {
    match code {
        Some(StatusCode::UidValidity(value)) => *uid_validity = Some(*value),
        Some(StatusCode::UidNext(value)) => status.uid_next = Some(*value),
        Some(StatusCode::Unseen(value)) => status.unseen = Some(*value),
        Some(StatusCode::HighestModSeq(value)) => status.highest_modseq = Some(*value),
        Some(StatusCode::ReadOnly) => status.read_only = true,
        Some(StatusCode::ReadWrite) => status.read_only = false,
        Some(StatusCode::PermanentFlags(flags)) => status.permanent_flags = flags.clone(),
        _ => {}
    }
}

/// Refuse a flag list that would be invalid or unsafe to write.
///
/// `\Recent` belongs to the server, and a hand-built [`Flag::Keyword`] is caller input: it must be
/// a single atom, never a space, a bracket or a CR/LF that could end the flag list.
fn check_storable_flags(flags: &[Flag]) -> Result<(), NetError> {
    if flags.is_empty() {
        return Err(protocol("no flags to store"));
    }
    if flags.len() > MAX_STORED_FLAGS {
        return Err(NetError::TooLarge { what: "flags", limit: MAX_STORED_FLAGS });
    }
    for flag in flags {
        match flag {
            Flag::Recent => return Err(protocol("\\Recent is set by the server and cannot be stored")),
            Flag::Keyword(keyword) if !crate::message::keyword_is_valid(keyword) => {
                return Err(protocol("a keyword contains characters that are not allowed in a flag"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A server `NO`/`BAD` during authentication is a credential problem, not a protocol one.
fn auth_error(error: NetError) -> NetError {
    match error {
        NetError::ServerNo { reason, .. } | NetError::ServerBad { reason, .. } => NetError::Auth { reason },
        other => other,
    }
}

fn is_bye(item: &Untagged) -> bool {
    matches!(item, Untagged::Status(status) if status.status == Status::Bye)
}

fn idle_event(item: Untagged) -> IdleEvent {
    match item {
        Untagged::Exists(count) => IdleEvent::Exists(count),
        Untagged::Expunge(sequence) => IdleEvent::Expunge(sequence),
        Untagged::Recent(count) => IdleEvent::Recent(count),
        Untagged::Status(status) if status.status == Status::Bye => IdleEvent::Bye(status.text),
        _ => IdleEvent::Other,
    }
}

fn build_command(tag: &str, verb: &str, arguments: Option<&str>, limit: usize) -> Result<String, NetError> {
    let length = tag.len() + 1 + verb.len() + arguments.map_or(0, |arguments| arguments.len() + 1);
    if length > limit {
        return Err(NetError::TooLarge { what: "command", limit });
    }
    let mut command = String::with_capacity(length + 2);
    command.push_str(tag);
    command.push(' ');
    command.push_str(verb);
    if let Some(arguments) = arguments {
        command.push(' ');
        command.push_str(arguments);
    }
    command.push_str("\r\n");
    Ok(command)
}

fn sasl_plain(username: &str, password: &str) -> Zeroizing<String> {
    let mut raw = Zeroizing::new(Vec::new());
    raw.push(0);
    raw.extend_from_slice(username.as_bytes());
    raw.push(0);
    raw.extend_from_slice(password.as_bytes());
    Zeroizing::new(STANDARD.encode(raw.as_slice()))
}

async fn resolve(host: &str, port: u16, after: Duration) -> Result<Vec<SocketAddr>, NetError> {
    match timeout(after, lookup_host((host, port))).await {
        Err(_) => Err(NetError::Timeout { phase: "dns", after }),
        Ok(Err(error)) => Err(NetError::Dns { host: host.to_owned(), reason: sanitize_reason(&error.to_string()) }),
        Ok(Ok(addresses)) => {
            let list: Vec<SocketAddr> = addresses.collect();
            if list.is_empty() {
                return Err(NetError::Dns { host: host.to_owned(), reason: "the resolver returned no addresses".to_owned() });
            }
            Ok(list)
        }
    }
}

/// Longest time spent on one resolved address when several are available.
const MAX_ATTEMPT: Duration = Duration::from_secs(5);

async fn connect_any(host: &str, port: u16, addresses: &[SocketAddr], timeout: Duration) -> Result<TcpStream, NetError> {
    let deadline = Instant::now().checked_add(timeout).unwrap_or_else(Instant::now);
    let mut last = "no address was tried".to_owned();
    let mut left = u32::try_from(addresses.len()).unwrap_or(u32::MAX).max(1);
    for address in addresses {
        let now = Instant::now();
        if now >= deadline {
            return Err(NetError::Timeout { phase: "connect", after: timeout });
        }
        // Give each address a fair share of the remaining budget, capped at five seconds: a host
        // whose IPv6 route is a black hole must not eat the timeout the IPv4 address needs.
        let share = deadline.saturating_duration_since(now) / left;
        let attempt_deadline = now.checked_add(share.min(MAX_ATTEMPT)).unwrap_or(deadline);
        left = left.saturating_sub(1).max(1);
        match timeout_at(attempt_deadline, TcpStream::connect(address)).await {
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(NetError::Timeout { phase: "connect", after: timeout });
                }
            }
            Ok(Err(error)) => last = sanitize_reason(&error.to_string()),
            Ok(Ok(stream)) => return Ok(stream),
        }
    }
    Err(NetError::Connect { host: host.to_owned(), port, reason: last })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detached_session() -> Session {
        let config = ImapConfig::new("imap.example.com", 993, TlsMode::Implicit, "user", Secret::new("pw"));
        let conn = Connection::new(Stream::Placeholder, config.limits.clamped(), config.timeout);
        Session { conn, tag: 0, capabilities: Capabilities::default(), authenticated: false, selected: None, config, closed: false }
    }

    #[test]
    fn tags_are_zero_padded_and_keep_growing() {
        let mut session = detached_session();
        assert_eq!(session.next_tag(), "A0001");
        assert_eq!(session.next_tag(), "A0002");
        session.tag = 9_999;
        assert_eq!(session.next_tag(), "A10000");
    }

    #[test]
    fn errors_that_desynchronise_the_stream_close_the_session() {
        let mut session = detached_session();
        for error in [
            NetError::Protocol { reason: "bad".to_owned() },
            NetError::TooLarge { what: "literal", limit: 8 },
            NetError::Timeout { phase: "list", after: Duration::from_millis(1) },
            NetError::Closed { phase: "list" },
            NetError::Tls { host: "imap.example.com".to_owned(), reason: "bad".to_owned() },
        ] {
            session.closed = false;
            session.note_failure(&error);
            assert!(session.closed, "{error:?} must close the session");
        }
        // A complete tagged NO leaves the stream at a command boundary: the caller may try again.
        session.closed = false;
        session.note_failure(&NetError::ServerNo { command: "SELECT", reason: "nope".to_owned() });
        assert!(!session.closed);
        session.note_failure(&NetError::Auth { reason: "nope".to_owned() });
        assert!(!session.closed);
    }

    #[test]
    fn a_closed_session_refuses_to_login_again() {
        let mut session = detached_session();
        session.closed = true;
        let error = poll_once(session.login()).expect_err("closed");
        assert!(matches!(error, NetError::Closed { phase: "auth" }), "{error:?}");
    }

    /// Poll one immediately-ready future without a runtime: `login` returns before its first await
    /// when the session is already closed.
    fn poll_once<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut context = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the future was expected to be ready"),
        }
    }

    #[test]
    fn commands_are_bounded() {
        let command = build_command("A0001", "NOOP", None, 64).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(command, "A0001 NOOP\r\n");
        assert!(matches!(build_command("A0001", "UID FETCH", Some(&"1".repeat(64)), 16), Err(NetError::TooLarge { .. })));
    }

    #[test]
    fn sasl_plain_payload_is_the_rfc_shape() {
        let payload = sasl_plain("user@example.com", "pw");
        let decoded = STANDARD.decode(payload.as_bytes()).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded, b"\0user@example.com\0pw");
    }

    #[test]
    fn storable_flags_are_validated_before_they_reach_the_wire() {
        assert!(check_storable_flags(&[]).is_err());
        assert!(check_storable_flags(&[Flag::Seen, Flag::Keyword("$Forwarded".to_owned())]).is_ok());
        assert!(check_storable_flags(&[Flag::Recent]).is_err());
        assert!(check_storable_flags(&[Flag::Keyword("has space".to_owned())]).is_err());
        assert!(check_storable_flags(&[Flag::Keyword("a\r\nB0001 NOOP".to_owned())]).is_err());
        let many = vec![Flag::Seen; MAX_STORED_FLAGS + 1];
        assert!(matches!(check_storable_flags(&many), Err(NetError::TooLarge { .. })));
    }
}
