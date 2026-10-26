//! Thread derivation and the cached conversation rows.
//!
//! A thread id is account-scoped: it is the root reference of the conversation, not a local
//! message id. The rules come from the P9b brief:
//!
//! 1. the first `References` entry that resolves to a cached thread,
//! 2. otherwise the first usable `References` entry (a phantom root waiting for its message),
//! 3. otherwise the thread of the `In-Reply-To` parent,
//! 4. otherwise `In-Reply-To` itself,
//! 5. otherwise the message's own `Message-ID`,
//! 6. otherwise the local message id.
//!
//! References are normalised (one angle-bracket pair removed), controls are refused, the chain is
//! capped at 32 entries of 512 bytes, duplicates are removed and a self-reference is ignored. A
//! reference that names an already cached message resolves through that message's thread, so a late
//! parent merges into the thread its replies already use. Two resolved threads merge into the
//! lexicographically smallest id; the other id becomes an alias and its rows are deleted. The merge
//! is therefore order-independent for the same set of candidates.
//!
//! When a message has no references at all, a reply/forward prefix and a normalised subject of at
//! least 8 bytes may attach it to exactly one thread whose messages carry the same subject within
//! 14 days. This is an attachment, never a merge: if two threads match, the fallback is dropped.
//!
//! The `threads` table is a cache: counts are mailbox-wide, the subject is the earliest message's
//! subject and the latest message id is the row a click opens. It is refreshed after every write
//! that can change a thread (insert, delete, flag, merge) and rebuilt from the message rows by
//! [`crate::Store::refresh_all_threads`].

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, params};

use crate::model::{Address, MAX_THREAD_LABELS, MAX_THREAD_PARTICIPANTS, ThreadDetail, ThreadPage, ThreadSummary};
use crate::repo::{SUMMARY_COLUMNS, corrupt_on_decode, get_u32, row_to_summary, to_count};
use crate::{Store, StoreError};

/// The longest reference the store accepts, in bytes. The sync layer already caps at this length;
/// the store repeats the check because it is the last boundary before an id becomes a cache key.
pub(crate) const MAX_THREAD_REFERENCE_BYTES: usize = 512;
/// The longest reference chain the store accepts.
pub(crate) const MAX_THREAD_REFERENCES: usize = 32;
/// The subject-fallback window, in milliseconds (14 days).
const SUBJECT_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// The shortest normalised subject the fallback may use, in bytes.
const MIN_SUBJECT_FALLBACK_BYTES: usize = 8;
/// Alias chains are bounded so a damaged table cannot loop.
const MAX_ALIAS_DEPTH: usize = 16;

/// A reference reduced to the exact identity the cache uses, or `None` when it is unusable.
pub(crate) fn normalise_thread_reference(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let inner = trimmed.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or(trimmed);
    if inner.is_empty() || inner.len() > MAX_THREAD_REFERENCE_BYTES {
        return None;
    }
    if inner.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    Some(inner.to_owned())
}

/// The bounded, de-duplicated chain a message owns. A reference equal to the message's own
/// `Message-ID` is ignored: a malformed header must not make a message cite itself.
pub(crate) fn normalise_chain(references: &[String], own_message_id: Option<&str>) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    for reference in references {
        if chain.len() >= MAX_THREAD_REFERENCES {
            break;
        }
        let Some(reference) = normalise_thread_reference(reference) else { continue };
        if own_message_id.is_some_and(|own| own == reference) {
            continue;
        }
        if !chain.contains(&reference) {
            chain.push(reference);
        }
    }
    chain
}

/// Resolve an id through the alias table. A damaged cycle stops at the first repeated id.
fn canonical_in_tx(tx: &Transaction<'_>, account_id: &str, id: &str) -> Result<String, StoreError> {
    let mut current = id.to_owned();
    let mut seen = BTreeSet::new();
    for _ in 0..MAX_ALIAS_DEPTH {
        if !seen.insert(current.clone()) {
            break;
        }
        let next: Option<String> = tx
            .query_row("SELECT new_id FROM thread_aliases WHERE account_id = ?1 AND old_id = ?2", params![account_id, current], |row| row.get(0))
            .optional()?;
        match next {
            Some(next) if next != current => current = next,
            _ => break,
        }
    }
    Ok(current)
}

/// The thread a reference denotes, if any message or cached thread already owns it.
///
/// A cached message wins over a thread row because it is the stronger fact; when several local rows
/// carry the same `Message-ID`, the smallest local id is the canonical copy the brief names.
fn resolve_reference_in_tx(
    tx: &Transaction<'_>,
    account_id: &str,
    reference: &str,
    exclude_local: Option<&str>,
) -> Result<Option<String>, StoreError> {
    let canonical = canonical_in_tx(tx, account_id, reference)?;
    let bracketed = format!("<{canonical}>");
    let message_thread: Option<String> = tx
        .query_row(
            "SELECT thread_id FROM messages
             WHERE account_id = ?1 AND message_id IN (?2, ?3) AND (?4 IS NULL OR id <> ?4)
             ORDER BY id ASC LIMIT 1",
            params![account_id, canonical, bracketed, exclude_local],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(thread) = message_thread {
        return canonical_in_tx(tx, account_id, &thread).map(Some);
    }
    let exists: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM threads WHERE account_id = ?1 AND id = ?2)", params![account_id, canonical], |row| row.get(0))?;
    if exists {
        return Ok(Some(canonical));
    }
    // A reference that is itself an alias may not have a row any more, but following the alias is
    // still the right answer: the target is the conversation.
    if canonical != reference {
        return Ok(Some(canonical));
    }
    Ok(None)
}

/// Point an unresolved reference at the conversation it belongs to. The reference had no message or
/// thread row yet, so there is nothing to move: only the alias is written, and a message that later
/// carries this id adopts the conversation instead of starting a second one.
fn alias_reference_in_tx(tx: &Transaction<'_>, account_id: &str, from: &str, into: &str) -> Result<(), StoreError> {
    if from == into {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO thread_aliases(account_id, old_id, new_id) VALUES(?1, ?2, ?3)
         ON CONFLICT(account_id, old_id) DO UPDATE SET new_id = excluded.new_id",
        params![account_id, from, into],
    )?;
    tx.execute("UPDATE thread_aliases SET new_id = ?3 WHERE account_id = ?1 AND new_id = ?2", params![account_id, from, into])?;
    Ok(())
}

/// Merge every message and alias from `from` into `into`. Both ids are canonical thread ids.
fn merge_threads_in_tx(tx: &Transaction<'_>, account_id: &str, from: &str, into: &str) -> Result<(), StoreError> {
    if from == into {
        return Ok(());
    }
    alias_reference_in_tx(tx, account_id, from, into)?;
    tx.execute("UPDATE messages SET thread_id = ?3 WHERE account_id = ?1 AND thread_id = ?2", params![account_id, from, into])?;
    tx.execute("DELETE FROM threads WHERE account_id = ?1 AND id = ?2", params![account_id, from])?;
    Ok(())
}

/// True when a subject starts with a reply or forward prefix, ignoring case and leading space.
fn has_reply_prefix(subject: &str) -> bool {
    let mut rest = subject.trim_start();
    let mut stripped = false;
    loop {
        let lower = rest.to_lowercase();
        let prefix = if lower.starts_with("re:") {
            3
        } else if lower.starts_with("fwd:") {
            4
        } else if lower.starts_with("fw:") {
            3
        } else {
            break;
        };
        let Some(after) = rest.get(prefix..) else { break };
        rest = after.trim_start();
        stripped = true;
        if rest.is_empty() {
            break;
        }
    }
    stripped
}

/// The one thread a subject fallback may attach to.
fn subject_candidate_in_tx(
    tx: &Transaction<'_>,
    account_id: &str,
    local_id: &str,
    date: i64,
    subject: &str,
    subject_norm: &str,
) -> Result<Option<String>, StoreError> {
    if !has_reply_prefix(subject) || subject_norm.len() < MIN_SUBJECT_FALLBACK_BYTES {
        return Ok(None);
    }
    let mut stmt = tx.prepare(
        "SELECT DISTINCT m.thread_id FROM messages m
         WHERE m.account_id = ?1 AND m.subject_norm = ?2 AND m.id <> ?3
           AND m.date BETWEEN ?4 AND ?5",
    )?;
    let rows = stmt.query_map(
        params![account_id, subject_norm, local_id, date.saturating_sub(SUBJECT_WINDOW_MS), date.saturating_add(SUBJECT_WINDOW_MS)],
        |row| row.get::<_, String>(0),
    )?;
    let mut threads = Vec::new();
    for row in rows {
        let thread = canonical_in_tx(tx, account_id, &row?)?;
        if !threads.contains(&thread) {
            threads.push(thread);
        }
        if threads.len() > 1 {
            // More than one conversation uses this subject: the fallback is not safe.
            return Ok(None);
        }
    }
    Ok(threads.into_iter().next())
}

/// Derive and assign the thread of one message that is already in the transaction, then refresh
/// the cache rows it touched. Returns the resolved thread id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assign_thread_in_tx(
    tx: &Transaction<'_>,
    account_id: &str,
    local_id: &str,
    own_message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    date: i64,
    subject: &str,
    subject_norm: &str,
) -> Result<String, StoreError> {
    let own = own_message_id.and_then(normalise_thread_reference);
    let chain = normalise_chain(references, own.as_deref());
    let parent = in_reply_to.and_then(normalise_thread_reference).filter(|parent| own.as_deref() != Some(parent));

    let mut candidates: Vec<String> = Vec::new();
    for reference in &chain {
        if let Some(thread) = resolve_reference_in_tx(tx, account_id, reference, None)? {
            let thread = canonical_in_tx(tx, account_id, &thread)?;
            if !candidates.contains(&thread) {
                candidates.push(thread);
            }
        }
    }
    if chain.is_empty()
        && let Some(parent) = &parent
        && let Some(thread) = resolve_reference_in_tx(tx, account_id, parent, None)?
    {
        let thread = canonical_in_tx(tx, account_id, &thread)?;
        if !candidates.contains(&thread) {
            candidates.push(thread);
        }
    }
    // A message adopts the phantom thread a reply already created for its own Message-ID. This is
    // what makes a late parent land in the same conversation as its children.
    if let Some(own) = &own
        && let Some(thread) = resolve_reference_in_tx(tx, account_id, own, Some(local_id))?
    {
        let thread = canonical_in_tx(tx, account_id, &thread)?;
        if !candidates.contains(&thread) {
            candidates.push(thread);
        }
    }

    let target = if let Some(smallest) = candidates.iter().min().cloned() {
        for other in &candidates {
            if *other != smallest {
                merge_threads_in_tx(tx, account_id, other, &smallest)?;
            }
        }
        smallest
    } else if let Some(first) = chain.first() {
        first.clone()
    } else if let Some(parent) = &parent {
        parent.clone()
    } else if let Some(candidate) = subject_candidate_in_tx(tx, account_id, local_id, date, subject, subject_norm)? {
        candidate
    } else if let Some(own) = own {
        own
    } else {
        local_id.to_owned()
    };

    // Every other reference names the same conversation. An unresolved one gets an alias now, so a
    // message that arrives later under that root joins this thread instead of starting a second one.
    for reference in chain.iter().chain(parent.iter()) {
        if reference == &target {
            continue;
        }
        if resolve_reference_in_tx(tx, account_id, reference, None)?.is_none() {
            alias_reference_in_tx(tx, account_id, reference, &target)?;
        }
    }

    let previous: Option<String> = tx
        .query_row("SELECT thread_id FROM messages WHERE id = ?1 AND account_id = ?2", params![local_id, account_id], |row| row.get(0))
        .optional()?;
    tx.execute("UPDATE messages SET thread_id = ?1 WHERE id = ?2 AND account_id = ?3", params![target, local_id, account_id])?;
    if let Some(previous) = previous
        && previous != target
    {
        refresh_thread_in_tx(tx, account_id, &previous)?;
    }
    refresh_thread_in_tx(tx, account_id, &target)?;
    Ok(target)
}

/// Recompute one cache row from the message rows. An empty thread disappears.
pub(crate) fn refresh_thread_in_tx(tx: &Transaction<'_>, account_id: &str, thread_id: &str) -> Result<(), StoreError> {
    let (count, unread, starred, attachments, last_date): (i64, i64, i64, i64, i64) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(unread), 0), COALESCE(SUM(starred), 0),
                COALESCE(MAX(has_attachments), 0), COALESCE(MAX(date), 0)
         FROM messages WHERE account_id = ?1 AND thread_id = ?2",
        params![account_id, thread_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    if count == 0 {
        tx.execute("DELETE FROM threads WHERE account_id = ?1 AND id = ?2", params![account_id, thread_id])?;
        return Ok(());
    }
    let (subject, latest): (String, Option<String>) = tx
        .query_row(
            "SELECT
                 COALESCE((SELECT subject FROM messages WHERE account_id = ?1 AND thread_id = ?2 ORDER BY date ASC, id ASC LIMIT 1), ''),
                 (SELECT id FROM messages WHERE account_id = ?1 AND thread_id = ?2 ORDER BY date DESC, id ASC LIMIT 1)",
            params![account_id, thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| corrupt_on_decode("threads", error))?;
    tx.execute(
        "INSERT INTO threads(account_id, id, subject, last_date, message_count, unread_count, starred_count, has_attachments, latest_message_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(account_id, id) DO UPDATE SET
             subject = excluded.subject,
             last_date = excluded.last_date,
             message_count = excluded.message_count,
             unread_count = excluded.unread_count,
             starred_count = excluded.starred_count,
             has_attachments = excluded.has_attachments,
             latest_message_id = excluded.latest_message_id",
        params![account_id, thread_id, subject, last_date, count, unread, starred, attachments != 0, latest],
    )?;
    Ok(())
}

/// Recompute the cache rows of several threads in one pass, ignoring duplicates.
pub(crate) fn refresh_threads_in_tx(tx: &Transaction<'_>, account_id: &str, thread_ids: impl IntoIterator<Item = String>) -> Result<(), StoreError> {
    let mut seen = BTreeSet::new();
    for thread_id in thread_ids {
        if seen.insert(thread_id.clone()) {
            refresh_thread_in_tx(tx, account_id, &thread_id)?;
        }
    }
    Ok(())
}

fn thread_scope_sql(filter: &crate::model::ThreadFilter, values: &mut Vec<rusqlite::types::Value>) -> String {
    let mut sql = String::from(" WHERE 1 = 1");
    if let Some(account) = &filter.account_id {
        sql.push_str(" AND t.account_id = ?");
        values.push(rusqlite::types::Value::Text(account.clone()));
    }
    let mut scope = String::new();
    let mut scope_values = Vec::new();
    if let Some(folder) = &filter.folder_id {
        scope.push_str(" AND m.folder_id = ?");
        scope_values.push(rusqlite::types::Value::Text(folder.clone()));
    }
    if let Some(label) = &filter.label_id {
        scope.push_str(" AND EXISTS(SELECT 1 FROM message_labels ml WHERE ml.message_id = m.id AND ml.label_id = ?)");
        scope_values.push(rusqlite::types::Value::Text(label.clone()));
    }
    if let Some(unread) = filter.unread {
        scope.push_str(" AND m.unread = ?");
        scope_values.push(rusqlite::types::Value::Integer(i64::from(unread)));
    }
    if let Some(starred) = filter.starred {
        scope.push_str(" AND m.starred = ?");
        scope_values.push(rusqlite::types::Value::Integer(i64::from(starred)));
    }
    if let Some(attachments) = filter.has_attachments {
        scope.push_str(" AND m.has_attachments = ?");
        scope_values.push(rusqlite::types::Value::Integer(i64::from(attachments)));
    }
    if !scope.is_empty() {
        // The scope is an uncorrelated set of thread ids, read from the covering list index: a
        // body page is never touched just to decide whether a conversation is in this folder.
        sql.push_str(" AND t.id IN (SELECT m.thread_id FROM messages m INDEXED BY messages_list_cover WHERE 1 = 1");
        if let Some(account) = &filter.account_id {
            sql.push_str(" AND m.account_id = ?");
            values.push(rusqlite::types::Value::Text(account.clone()));
        }
        sql.push_str(&scope);
        sql.push(')');
        values.extend(scope_values);
    }
    sql
}

fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSummary> {
    let unread = get_u32(row, 6)?;
    Ok(ThreadSummary {
        account_id: row.get(0)?,
        id: row.get(1)?,
        subject: row.get(2)?,
        snippet: row.get(3)?,
        participants: Vec::new(),
        last_date: row.get(4)?,
        message_count: get_u32(row, 5)?,
        unread_count: unread,
        starred_count: get_u32(row, 7)?,
        has_attachments: row.get(8)?,
        latest_message_id: row.get(9)?,
        labels: Vec::new(),
        state: if unread > 0 { "unread".to_owned() } else { "read".to_owned() },
    })
}

impl Store {
    /// Threads with at least one message in the filter's scope. Counts stay mailbox-wide.
    pub fn list_threads(&self, filter: &crate::model::ThreadFilter) -> Result<ThreadPage, StoreError> {
        let mut values = Vec::new();
        let where_sql = thread_scope_sql(filter, &mut values);
        let (total, unread_threads, unread_messages): (i64, i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN t.unread_count > 0 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(t.unread_count), 0)
                 FROM threads t{where_sql}"
            ),
            rusqlite::params_from_iter(values.iter()),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let limit = filter.limit.clamp(1, 200);
        let mut page_values = values;
        page_values.push(rusqlite::types::Value::Integer(i64::from(limit)));
        page_values.push(rusqlite::types::Value::Integer(i64::from(filter.offset.min(5_000))));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.account_id, t.id, t.subject, '' AS snippet, t.last_date,
                    t.message_count, t.unread_count, t.starred_count, t.has_attachments, t.latest_message_id
             FROM threads t
             {where_sql}
             ORDER BY t.last_date DESC, t.id ASC
             LIMIT ? OFFSET ?"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(page_values.iter()), row_to_thread)?;
        let mut threads = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("threads", error))?;
        drop(stmt);
        self.fill_thread_facts(&mut threads)?;
        Ok(ThreadPage { threads, total: to_count(total)?, unread_threads: to_count(unread_threads)?, unread_messages: to_count(unread_messages)? })
    }

    /// Add participants and the label union to a page of thread rows. Both are computed for the
    /// page only, so a 100k mailbox never pays for facts nobody sees. Threat ids are account-scoped,
    /// so the facts are queried per account: the same `Message-ID` in two accounts must not mix.
    fn fill_thread_facts(&self, threads: &mut [ThreadSummary]) -> Result<(), StoreError> {
        if threads.is_empty() {
            return Ok(());
        }
        // The latest message's snippet is one PK lookup per page row. It is fetched after the LIMIT,
        // never joined into the ordered scan: joining the 100k-row cache to the message table would
        // read every message row before the page was taken.
        let latest = threads.iter().filter_map(|thread| thread.latest_message_id.clone()).collect::<Vec<_>>();
        if !latest.is_empty() {
            let placeholders = vec!["?"; latest.len()].join(",");
            let mut stmt = self.conn.prepare(&format!("SELECT id, snippet FROM messages WHERE id IN ({placeholders})"))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(latest.iter()), |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, snippet) = row?;
                if let Some(thread) = threads.iter_mut().find(|thread| thread.latest_message_id.as_deref() == Some(id.as_str())) {
                    thread.snippet = snippet;
                }
            }
        }
        let mut accounts: Vec<String> = Vec::new();
        for thread in threads.iter() {
            if !accounts.contains(&thread.account_id) {
                accounts.push(thread.account_id.clone());
            }
        }
        for account in accounts {
            let ids = threads.iter().filter(|thread| thread.account_id == account).map(|thread| thread.id.clone()).collect::<Vec<_>>();
            if ids.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; ids.len()].join(",");
            let mut values = vec![rusqlite::types::Value::Text(account.clone())];
            values.extend(ids.iter().cloned().map(rusqlite::types::Value::Text));
            {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT thread_id, date, id, from_name, from_email FROM messages INDEXED BY messages_account_thread_date
                     WHERE account_id = ?1 AND thread_id IN ({placeholders})
                     LIMIT 2000"
                ))?;
                let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, Address { name: row.get(3)?, email: row.get(4)? }))
                })?;
                let mut found = Vec::new();
                for row in rows {
                    found.push(row?);
                }
                found.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)).then(left.2.cmp(&right.2)));
                for (thread_id, _, _, address) in found {
                    let Some(thread) = threads.iter_mut().find(|thread| thread.account_id == account && thread.id == thread_id) else {
                        continue;
                    };
                    if thread.participants.len() < MAX_THREAD_PARTICIPANTS
                        && !thread.participants.iter().any(|known| known.email.eq_ignore_ascii_case(&address.email))
                    {
                        thread.participants.push(address);
                    }
                }
            }
            {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT m.thread_id, ml.label_id FROM messages m INDEXED BY messages_account_thread_date
                     JOIN message_labels ml ON ml.message_id = m.id
                     WHERE m.account_id = ?1 AND m.thread_id IN ({placeholders})
                     LIMIT 4000"
                ))?;
                let rows =
                    stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                let mut found = Vec::new();
                for row in rows {
                    found.push(row?);
                }
                found.sort();
                found.dedup();
                for (thread_id, label) in found {
                    let Some(thread) = threads.iter_mut().find(|thread| thread.account_id == account && thread.id == thread_id) else {
                        continue;
                    };
                    if thread.labels.len() < MAX_THREAD_LABELS && !thread.labels.contains(&label) {
                        thread.labels.push(label);
                    }
                }
            }
        }
        Ok(())
    }

    /// One thread with an ascending page of its messages. `id` may be an old merged id.
    pub fn get_thread(&self, account_id: &str, id: &str, limit: u32, offset: u32) -> Result<Option<ThreadDetail>, StoreError> {
        let canonical = self.canonical_thread(account_id, id)?;
        // The filter cannot select one id directly, so read the row and the messages separately.
        let mut stmt = self.conn.prepare(
            "SELECT t.account_id, t.id, t.subject, '' AS snippet, t.last_date,
                    t.message_count, t.unread_count, t.starred_count, t.has_attachments, t.latest_message_id
             FROM threads t
             WHERE t.account_id = ?1 AND t.id = ?2",
        )?;
        let summary =
            stmt.query_row(params![account_id, canonical], row_to_thread).optional().map_err(|error| corrupt_on_decode("threads", error))?;
        drop(stmt);
        let Some(mut thread) = summary else { return Ok(None) };
        self.fill_thread_facts(std::slice::from_mut(&mut thread))?;

        let total: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM messages WHERE account_id = ?1 AND thread_id = ?2", params![account_id, canonical], |row| {
                row.get(0)
            })?;
        let limit = limit.clamp(1, 200);
        let offset = offset.min(5_000);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m
             WHERE m.account_id = ?1 AND m.thread_id = ?2
             ORDER BY m.date ASC, m.id ASC LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(params![account_id, canonical, limit, offset], row_to_summary)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let labels = self.labels_by_message(&messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>())?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.id) {
                message.labels.clone_from(ids);
            }
        }
        Ok(Some(ThreadDetail { thread, messages, total: to_count(total)? }))
    }

    /// Every message id in one thread, ascending. The caller enforces the 500-row mutation cap.
    pub fn thread_message_ids(&self, account_id: &str, id: &str) -> Result<Vec<String>, StoreError> {
        let canonical = self.canonical_thread(account_id, id)?;
        let mut stmt = self.conn.prepare("SELECT id FROM messages WHERE account_id = ?1 AND thread_id = ?2 ORDER BY date ASC, id ASC")?;
        let rows = stmt.query_map(params![account_id, canonical], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The local flags of every message in one thread, ascending. The thread flag operation compares
    /// this snapshot before the write, so `changed` counts real transitions and a repeat returns 0.
    pub fn thread_flags(&self, account_id: &str, id: &str) -> Result<Vec<(String, bool, bool)>, StoreError> {
        let canonical = self.canonical_thread(account_id, id)?;
        let mut stmt =
            self.conn.prepare("SELECT id, unread, starred FROM messages WHERE account_id = ?1 AND thread_id = ?2 ORDER BY date ASC, id ASC")?;
        let rows = stmt.query_map(params![account_id, canonical], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Follow the alias table from a possibly old id to the current one.
    pub fn canonical_thread(&self, account_id: &str, id: &str) -> Result<String, StoreError> {
        let mut canonical = id.to_owned();
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_ALIAS_DEPTH {
            if !seen.insert(canonical.clone()) {
                break;
            }
            let next: Option<String> = self
                .conn
                .query_row("SELECT new_id FROM thread_aliases WHERE account_id = ?1 AND old_id = ?2", params![account_id, canonical], |row| {
                    row.get(0)
                })
                .optional()?;
            match next {
                Some(next) if next != canonical => canonical = next,
                _ => break,
            }
        }
        Ok(canonical)
    }

    /// The thread a `Message-ID` belongs to, without writing anything. A locally sent message uses
    /// this before its row is written so a later sync of the same root derives the same id.
    pub fn thread_id_for_message_id(&self, account_id: &str, message_id: &str) -> Result<Option<String>, StoreError> {
        let canonical = self.canonical_thread(account_id, message_id)?;
        let bracketed = format!("<{canonical}>");
        let thread: Option<String> = self
            .conn
            .query_row(
                "SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id IN (?2, ?3) ORDER BY id ASC LIMIT 1",
                params![account_id, canonical, bracketed],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(thread) = thread {
            return self.canonical_thread(account_id, &thread).map(Some);
        }
        let exists: bool =
            self.conn.query_row("SELECT EXISTS(SELECT 1 FROM threads WHERE account_id = ?1 AND id = ?2)", params![account_id, canonical], |row| {
                row.get(0)
            })?;
        if exists {
            return Ok(Some(canonical));
        }
        Ok(None)
    }

    /// Rebuild every thread cache row from the message rows, in one transaction.
    ///
    /// The insert computes the mailbox-wide counts and the latest date directly in SQL. The subject
    /// and the latest message id are two indexed lookups per thread; a thread with no message is not
    /// inserted at all, which is what keeps Phantoms from outliving their conversation.
    pub fn refresh_all_threads(&self) -> Result<u32, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM threads", [])?;
        tx.execute(
            "INSERT INTO threads(account_id, id, subject, last_date, message_count, unread_count, starred_count, has_attachments, latest_message_id)
             SELECT m.account_id, m.thread_id,
                    COALESCE((SELECT s.subject FROM messages s WHERE s.account_id = m.account_id AND s.thread_id = m.thread_id
                              ORDER BY s.date ASC, s.id ASC LIMIT 1), ''),
                    MAX(m.date), COUNT(*), COALESCE(SUM(m.unread), 0), COALESCE(SUM(m.starred), 0),
                    COALESCE(MAX(m.has_attachments), 0),
                    (SELECT l.id FROM messages l WHERE l.account_id = m.account_id AND l.thread_id = m.thread_id
                     ORDER BY l.date DESC, l.id ASC LIMIT 1)
             FROM messages m GROUP BY m.account_id, m.thread_id",
            [],
        )?;
        let threads: i64 = tx.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        tx.commit()?;
        to_count(threads)
    }

    /// Create the cache from existing messages once, for a store written by P9a. A populated cache
    /// is left alone: it is maintained incrementally and a rebuild is an explicit maintenance step.
    pub fn ensure_thread_cache(&self) -> Result<bool, StoreError> {
        let threads: i64 = self.conn.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
        if threads > 0 {
            return Ok(false);
        }
        let messages: i64 = self.conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        if messages == 0 {
            return Ok(false);
        }
        self.refresh_all_threads()?;
        Ok(true)
    }
}
