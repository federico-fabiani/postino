//! The local correspondent list: one row per address this account has received mail from or
//! alongside, ordered by the newest message that carried it.
//!
//! It answers two questions that would otherwise scan the mailbox on every keystroke: who wrote to
//! me recently (the composer's recipient autocomplete) and which address a prefix names (`from:` in
//! the search bar). It is derived data — [`note_received`] maintains it inside the message
//! transaction, and [`ensure_correspondents`] backfills an archive written before v10 exactly once.
//! Only received mail feeds it, and the account's own address is never stored, so a reply never
//! suggests the person who sent it.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Store;
use crate::model::{Address, Message};

/// Most rows one suggestion query returns.
pub const MAX_CORRESPONDENTS: usize = 50;

/// One address the account has corresponded with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Correspondent {
    /// Lowercased address, without a display name.
    pub address: String,
    /// The most recent non-empty display name, or `""`.
    pub name: String,
    /// The date of the newest message that carried this address.
    pub last_date: i64,
}

/// Record the people on one received message. Called inside the message insert transaction, so the
/// list can never be newer or older than the mail it describes.
pub(crate) fn note_received(conn: &Connection, message: &Message) -> Result<(), crate::StoreError> {
    let own: Option<String> =
        conn.query_row("SELECT email FROM accounts WHERE id = ?1", [&message.summary.account_id], |row| row.get(0)).optional()?;
    let own = own.unwrap_or_default().trim().to_ascii_lowercase();
    let account = message.summary.account_id.as_str();
    let date = message.summary.date;
    // The sender first: on a same-date tie its display name wins over a recipient spelling.
    note(conn, account, &message.summary.from, date, &own)?;
    for address in message.summary.to.iter().chain(message.cc.iter()) {
        note(conn, account, address, date, &own)?;
    }
    Ok(())
}

fn note(conn: &Connection, account_id: &str, address: &Address, date: i64, own: &str) -> Result<(), crate::StoreError> {
    let email = address.email.trim().to_ascii_lowercase();
    // A malformed header is skipped instead of stored: a suggestion list is not a place to invent
    // an address, and an empty string would sort first forever.
    if email.is_empty() || !email.contains('@') || email == own {
        return Ok(());
    }
    upsert(conn, account_id, &email, address.name.trim(), date)
}

/// Insert or refresh one row. A newer message moves `last_date` forward and replaces the name, but
/// never with an empty one: a sender who dropped the display name keeps the name we already knew.
fn upsert(conn: &Connection, account_id: &str, address: &str, name: &str, date: i64) -> Result<(), crate::StoreError> {
    conn.execute(
        "INSERT INTO correspondents(account_id, address, name, last_date)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, address) DO UPDATE SET
             name = CASE
                        WHEN excluded.last_date > correspondents.last_date AND excluded.name <> ''
                            THEN excluded.name
                        ELSE correspondents.name
                    END,
             last_date = max(correspondents.last_date, excluded.last_date)",
        params![account_id, address, name, date],
    )?;
    Ok(())
}

impl Store {
    /// Addresses for `account_id` whose name or address contains `query`, newest first.
    ///
    /// `query` is a plain substring, never SQL and never a LIKE pattern: `%`, `_` and `\` are
    /// escaped. An empty query returns the most recent rows, which is what an empty composer
    /// field wants.
    pub fn correspondents(&self, account_id: &str, query: &str, limit: usize) -> Result<Vec<Correspondent>, crate::StoreError> {
        let limit = limit.clamp(1, MAX_CORRESPONDENTS);
        let pattern = format!("%{}%", escape_like(query.trim()));
        let mut stmt = self.conn.prepare(
            "SELECT address, name, last_date
               FROM correspondents
              WHERE account_id = ?1
                AND (address LIKE ?2 ESCAPE '\\' OR name LIKE ?2 ESCAPE '\\')
              ORDER BY last_date DESC, address
              LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, pattern, limit as i64], |row| {
            Ok(Correspondent { address: row.get(0)?, name: row.get(1)?, last_date: row.get(2)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// `meta` key proving the archive has been read once. The backfill is idempotent, but a store with
/// a hundred thousand messages must not re-read them on every open.
const BACKFILLED_KEY: &str = "correspondents_backfilled";

/// Fill the list from mail cached before v10, once per store.
///
/// Reads every received message in date order and keeps the newest name per address, then writes one
/// row per distinct address inside a single transaction. A damaged recipient list is skipped rather
/// than failing the open: the message is still readable and a later sync refreshes its rows.
pub(crate) fn ensure_correspondents(conn: &mut Connection) -> Result<(), crate::StoreError> {
    if conn.query_row("SELECT 1 FROM meta WHERE key = ?1", [BACKFILLED_KEY], |_| Ok(())).optional()?.is_some() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut own: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT id, email FROM accounts")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, email) = row?;
                own.insert(id, email.trim().to_ascii_lowercase());
            }
        }
        let mut seen: HashMap<(String, String), (String, i64)> = HashMap::new();
        {
            // Ascending date: the last name written for an address is the newest one.
            let mut stmt = tx.prepare(
                "SELECT account_id, date, from_name, from_email, to_json, cc_json
                   FROM messages
                  WHERE origin = 'received'
                  ORDER BY date",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let account: String = row.get(0)?;
                let date: i64 = row.get(1)?;
                let from_name: String = row.get(2)?;
                let from_email: String = row.get(3)?;
                let to_json: String = row.get(4)?;
                let cc_json: String = row.get(5)?;
                let own_address = own.get(&account).cloned().unwrap_or_default();
                let mut people = vec![(from_name, from_email)];
                for json in [to_json, cc_json] {
                    if let Ok(list) = serde_json::from_str::<Vec<Address>>(&json) {
                        people.extend(list.into_iter().map(|address| (address.name, address.email)));
                    }
                }
                for (name, email) in people {
                    let email = email.trim().to_ascii_lowercase();
                    if email.is_empty() || !email.contains('@') || email == own_address {
                        continue;
                    }
                    let entry = seen.entry((account.clone(), email)).or_insert_with(|| (String::new(), date));
                    if !name.trim().is_empty() {
                        entry.0 = name.trim().to_owned();
                    }
                    entry.1 = date;
                }
            }
        }
        for ((account, address), (name, date)) in seen {
            upsert(&tx, &account, &address, &name, date)?;
        }
    }
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES(?1, '1')", [BACKFILLED_KEY])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, Folder, FolderKind, Message, MessageSummary};
    use crate::{Store, StoreKey};

    const ACCOUNT: &str = "acc_1";
    const INBOX: &str = "inbox";
    const SENT: &str = "sent";

    struct Fixture {
        dir: tempfile::TempDir,
        key: StoreKey,
    }

    impl Fixture {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().expect("temp dir"), key: StoreKey::from_bytes([0x5a; 32]) }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("store.db")
        }

        fn open(&self) -> Store {
            Store::open(&self.path(), &self.key).expect("open the store")
        }
    }

    fn seed(store: &Store) {
        store
            .upsert_account(&Account {
                id: ACCOUNT.to_owned(),
                name: "Studio Verdi".to_owned(),
                email: "studio@example.com".to_owned(),
                provider: "demo".to_owned(),
                signature: String::new(),
                created_at: 1,
            })
            .expect("account");
        for (id, name, kind) in [(INBOX, "Posta in arrivo", FolderKind::Inbox), (SENT, "Inviata", FolderKind::Sent)] {
            store
                .upsert_folder(&Folder {
                    id: id.to_owned(),
                    account_id: ACCOUNT.to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: Some(1),
                    uid_next: Some(10),
                    last_sync_at: None,
                })
                .expect("folder");
        }
    }

    fn message(id: &str, folder: &str, from: (&str, &str), to: &[(&str, &str)], cc: &[(&str, &str)], date: i64) -> Message {
        Message {
            summary: MessageSummary {
                id: id.to_owned(),
                account_id: ACCOUNT.to_owned(),
                folder_id: folder.to_owned(),
                thread_id: format!("thread_{id}"),
                from: Address::new(from.0, from.1),
                to: to.iter().map(|(name, email)| Address::new(*name, *email)).collect(),
                subject: "Oggetto".to_owned(),
                snippet: String::new(),
                date,
                unread: false,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: Some(1),
            message_id: None,
            cc: cc.iter().map(|(name, email)| Address::new(*name, *email)).collect(),
            bcc: Vec::new(),
            body_text: "Corpo".to_owned(),
            body_html: None,
            remote_images: 0,
            size: None,
        }
    }

    fn addresses(list: &[Correspondent]) -> Vec<&str> {
        list.iter().map(|row| row.address.as_str()).collect()
    }

    #[test]
    fn received_mail_records_its_people_newest_first_and_never_the_account_itself() {
        let fixture = Fixture::new();
        let store = fixture.open();
        seed(&store);
        store
            .upsert_message(&message(
                "msg_1",
                INBOX,
                ("Giulia Rossi", "Giulia@Example.com"),
                &[("Studio Verdi", "studio@example.com")],
                &[("Marco", "marco@example.it")],
                100,
            ))
            .expect("first message");
        store
            .upsert_message(&message(
                "msg_2",
                INBOX,
                ("Federica Bianchi", "federica@example.com"),
                &[("Giulia Rossi", "giulia@example.com")],
                &[],
                200,
            ))
            .expect("second message");

        let all = store.correspondents(ACCOUNT, "", 20).expect("correspondents");
        assert_eq!(addresses(&all), vec!["federica@example.com", "giulia@example.com", "marco@example.it"]);
        assert_eq!(all[1].name, "Giulia Rossi", "the newest spelling of a name wins");
        assert_eq!(all[1].last_date, 200);
        assert!(!addresses(&all).contains(&"studio@example.com"), "the account's own address is not a correspondent");

        // A sent copy is not a person to offer as a recipient.
        store
            .upsert_message(&message("msg_3", SENT, ("Studio Verdi", "studio@example.com"), &[("Nuovo", "nuovo@example.it")], &[], 300))
            .expect("sent copy");
        assert!(!addresses(&store.correspondents(ACCOUNT, "", 20).expect("after sent")).contains(&"nuovo@example.it"));
    }

    #[test]
    fn a_query_is_a_plain_substring_and_wildcards_are_escaped() {
        let fixture = Fixture::new();
        let store = fixture.open();
        seed(&store);
        store
            .upsert_message(&message("msg_1", INBOX, ("Giulia Rossi", "giulia@example.com"), &[("Marco", "marco@example.it")], &[], 100))
            .expect("message");

        assert_eq!(addresses(&store.correspondents(ACCOUNT, "giu", 20).expect("prefix")), vec!["giulia@example.com"]);
        assert_eq!(addresses(&store.correspondents(ACCOUNT, "MARCO", 20).expect("case-insensitive")), vec!["marco@example.it"]);
        assert!(store.correspondents(ACCOUNT, "%", 20).expect("escaped wildcard").is_empty(), "a bare % is not a wildcard");
        assert!(store.correspondents(ACCOUNT, "_", 20).expect("escaped wildcard").is_empty(), "a bare _ is not a wildcard");
        assert_eq!(store.correspondents(ACCOUNT, "", 1).expect("bounded").len(), 1, "the limit is honoured");
    }

    #[test]
    fn an_empty_name_never_erases_the_name_already_known() {
        let fixture = Fixture::new();
        let store = fixture.open();
        seed(&store);
        store.upsert_message(&message("msg_1", INBOX, ("Giulia Rossi", "giulia@example.com"), &[], &[], 100)).expect("named");
        store.upsert_message(&message("msg_2", INBOX, ("", "giulia@example.com"), &[], &[], 200)).expect("unnamed, newer");
        let all = store.correspondents(ACCOUNT, "giulia", 5).expect("correspondents");
        assert_eq!(all[0].name, "Giulia Rossi");
        assert_eq!(all[0].last_date, 200);
    }

    #[test]
    fn an_archive_written_before_v10_is_backfilled_once() {
        let fixture = Fixture::new();
        let store = fixture.open();
        seed(&store);
        store
            .upsert_message(&message("msg_1", INBOX, ("Giulia Rossi", "giulia@example.com"), &[("Marco", "marco@example.it")], &[], 100))
            .expect("message");
        // Pretend the archive predates the list: the table is empty and the one-time flag is absent.
        store.conn.execute("DELETE FROM correspondents", []).expect("clear the list");
        store.delete_meta(BACKFILLED_KEY).expect("clear the flag");
        assert!(store.correspondents(ACCOUNT, "", 20).expect("empty").is_empty());

        let reopened = fixture.open();
        assert_eq!(addresses(&reopened.correspondents(ACCOUNT, "", 20).expect("backfilled")), vec!["giulia@example.com", "marco@example.it"]);
        // A second open does not need to read the mailbox again: the flag is written.
        assert_eq!(reopened.get_meta(BACKFILLED_KEY).expect("meta").as_deref(), Some("1"));
    }
}
