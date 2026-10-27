//! FTS5 search over the encrypted store.
//!
//! The query language lives in `postino-core`; this crate receives a typed [`SearchExpr`] tree in
//! which every text leaf is a complete, already-quoted FTS5 expression. The store compiles that
//! tree into fixed SQL fragments with bound parameters. No caller string is ever concatenated into
//! SQL, and `MATCH` always receives a bound value.
//!
//! The index is contentless-delete: it stores terms for `subject`, people, attachment names and at
//! most 1 MiB of plain body text, but no content. Snippets therefore come from the message row, not
//! from FTS, which is also why an HTML marker can never leak into a search result.

use std::time::Instant;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::model::{SEARCH_TOTAL_CAP, SearchExpr, SearchFilter, SearchHit, SearchIndexStatus, SearchPage, SearchReindexOutcome, SearchSort};
use crate::repo::{SUMMARY_COLUMNS, row_to_summary, row_to_summary_at};
use crate::{Store, StoreError};

/// `meta` key holding the resumable index state.
pub const SEARCH_INDEX_KEY: &str = "search_index";
/// `meta` key stamped by the FTS triggers on every index mutation, so status means "last index
/// update", not "last full build".
pub const SEARCH_UPDATED_KEY: &str = "search_updated_at";

/// Rows indexed per resumable batch. A batch is one transaction, so a crash resumes after the last
/// committed batch and never leaves a half-updated row.
pub const SEARCH_BATCH: u32 = 500;

/// Characters of `body_text` copied into FTS. At four bytes per UTF-8 scalar it is at most 1 MiB;
/// the repository never indexes more of a hostile body.
const BODY_INDEX_CHARS: u32 = 262_144;

/// Characters of `body_text` returned per hit for Rust snippets (64 KiB at most).
const SNIPPET_SOURCE_CHARS: u32 = 16_384;

/// Largest page a search can return.
const MAX_HITS: u32 = 200;

/// Largest offset a caller can ask for.
const MAX_OFFSET: u32 = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexMeta {
    state: String,
    last_rowid: i64,
    indexed: u32,
    total: u32,
    updated_at: i64,
    last_error: Option<String>,
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self { state: "pending".to_owned(), last_rowid: 0, indexed: 0, total: 0, updated_at: 0, last_error: None }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct IndexHealth {
    indexed: u32,
    total: u32,
    missing: u32,
    extra: u32,
    rank_missing: u32,
    rank_extra: u32,
}

impl IndexHealth {
    fn drift(self) -> bool {
        self.missing > 0 || self.extra > 0 || self.rank_missing > 0 || self.rank_extra > 0
    }
}

impl Store {
    /// One search page. Filter-only queries skip `MATCH`; text queries use a rank CTE so bm25 is
    /// computed once for the whole query and a stable `(date DESC, id ASC)` tie-break decides ties.
    pub fn search(&self, filter: &SearchFilter) -> Result<SearchPage, StoreError> {
        let limit = filter.limit.clamp(1, MAX_HITS);
        let offset = filter.offset.min(MAX_OFFSET);
        // A query made only of text terms and boolean operators can use one FTS join and one MATCH
        // parameter. That is the common search path and the reason the first page stays fast.
        if let Some(fts) = filter.expr.as_ref().and_then(pure_fts_expression) {
            return self.search_fts(filter, &fts, limit, offset);
        }
        let mut values = Vec::new();
        let where_sql = match &filter.expr {
            Some(expr) => {
                let mut depth = 0u8;
                let sql = expr_sql(expr, &mut values, &mut depth)?;
                if let Some(account) = &filter.account_id {
                    values.insert(0, Value::Text(account.clone()));
                    format!("m.account_id = ? AND ({sql})")
                } else {
                    sql
                }
            }
            None => match &filter.account_id {
                Some(account) => {
                    values.push(Value::Text(account.clone()));
                    "m.account_id = ?".to_owned()
                }
                None => "1 = 1".to_owned(),
            },
        };

        let mut count_values = values.clone();
        count_values.push(Value::Integer(i64::from(SEARCH_TOTAL_CAP) + 1));
        let counted: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT m.id FROM messages m WHERE {where_sql} LIMIT ?)"),
            params_from_iter(count_values.iter()),
            |row| row.get(0),
        )?;
        let total_capped = counted > i64::from(SEARCH_TOTAL_CAP);
        let total = u32::try_from(counted.min(i64::from(SEARCH_TOTAL_CAP))).unwrap_or(SEARCH_TOTAL_CAP);

        let rank = filter.expr.as_ref().and_then(positive_match_expression);
        let mut page_values = Vec::new();
        let (cte, rank_expr) = match &rank {
            Some(rank) => {
                page_values.push(Value::Text(rank.clone()));
                (
                    "WITH ranked AS (
                         SELECT rowid,
                                bm25(message_fts, 4.0, 3.0, 2.0, 1.0, 0.5, 1.0, 2.0) AS rank
                         FROM message_fts
                         WHERE message_fts MATCH ?
                     )"
                    .to_owned(),
                    "COALESCE(r.rank, 4.0) ASC, m.date DESC, m.id ASC",
                )
            }
            None => ("".to_owned(), "m.date DESC, m.id ASC"),
        };
        let order = if rank.is_some() && filter.sort == SearchSort::Relevance { rank_expr } else { "m.date DESC, m.id ASC" };
        let join = if rank.is_some() { " LEFT JOIN ranked r ON r.rowid = m.rowid" } else { "" };
        page_values.extend(values);
        page_values.push(Value::Integer(i64::from(limit)));
        page_values.push(Value::Integer(i64::from(offset)));
        let sql = format!(
            "{cte} SELECT {SUMMARY_COLUMNS}, substr(m.body_text, 1, {SNIPPET_SOURCE_CHARS}), {rank_value}
             FROM messages m{join}
             WHERE {where_sql}
             ORDER BY {order}
             LIMIT ? OFFSET ?",
            rank_value = if rank.is_some() { "COALESCE(r.rank, 4.0)" } else { "0.0" },
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(page_values.iter()), |row| {
            let message = row_to_summary(row)?;
            let body_text: String = row.get(13)?;
            let rank: f64 = row.get(14)?;
            Ok(SearchHit { message, body_text, rank })
        })?;
        let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.attach_labels(&mut hits)?;
        Ok(SearchPage { hits, total, total_capped })
    }

    /// The common path: one `message_fts` join, one bound `MATCH`, bm25 from the same row. It avoids
    /// a per-leaf subquery and a rank CTE for a query that is only text.
    fn search_fts(&self, filter: &SearchFilter, fts: &str, limit: u32, offset: u32) -> Result<SearchPage, StoreError> {
        if filter.account_id.is_none() && filter.sort == SearchSort::Relevance {
            return self.search_fts_relevance(fts, limit, offset);
        }
        let (where_sql, mut values) = match &filter.account_id {
            Some(account) => ("m.account_id = ? AND message_fts MATCH ?".to_owned(), vec![Value::Text(account.clone()), Value::Text(fts.to_owned())]),
            None => ("message_fts MATCH ?".to_owned(), vec![Value::Text(fts.to_owned())]),
        };
        let mut count_values = values.clone();
        count_values.push(Value::Integer(i64::from(SEARCH_TOTAL_CAP) + 1));
        let counted: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT m.id FROM messages m JOIN message_fts ON message_fts.rowid = m.rowid WHERE {where_sql} LIMIT ?)"),
            params_from_iter(count_values.iter()),
            |row| row.get(0),
        )?;
        let total_capped = counted > i64::from(SEARCH_TOTAL_CAP);
        let total = u32::try_from(counted.min(i64::from(SEARCH_TOTAL_CAP))).unwrap_or(SEARCH_TOTAL_CAP);
        let order = if filter.sort == SearchSort::Relevance {
            "bm25(message_fts, 4.0, 3.0, 2.0, 1.0, 0.5, 1.0, 2.0) ASC, m.date DESC, m.id ASC"
        } else {
            "m.date DESC, m.id ASC"
        };
        values.push(Value::Integer(i64::from(limit)));
        values.push(Value::Integer(i64::from(offset)));
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS}, substr(m.body_text, 1, {SNIPPET_SOURCE_CHARS}),
                    bm25(message_fts, 4.0, 3.0, 2.0, 1.0, 0.5, 1.0, 2.0)
             FROM messages m JOIN message_fts ON message_fts.rowid = m.rowid
             WHERE {where_sql}
             ORDER BY {order}
             LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            let message = row_to_summary(row)?;
            let body_text: String = row.get(13)?;
            let rank: f64 = row.get(14)?;
            Ok(SearchHit { message, body_text, rank })
        })?;
        let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.attach_labels(&mut hits)?;
        Ok(SearchPage { hits, total, total_capped })
    }

    /// Relevance over a single-account text query: rank and page inside FTS5, with the date/id
    /// tie-break in an auxiliary rank table maintained by the same triggers. The SQL order is global
    /// and exact; only the selected page is joined back to the encrypted message rows.
    fn search_fts_relevance(&self, fts: &str, limit: u32, offset: u32) -> Result<SearchPage, StoreError> {
        let count_values = [Value::Text(fts.to_owned()), Value::Integer(i64::from(SEARCH_TOTAL_CAP) + 1)];
        let counted: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT f.rowid FROM message_fts f
                 JOIN message_fts_rank r ON r.rowid = f.rowid
                 WHERE message_fts MATCH ?1 LIMIT ?2
             )",
            params_from_iter(count_values.iter()),
            |row| row.get(0),
        )?;
        let total_capped = counted > i64::from(SEARCH_TOTAL_CAP);
        let total = u32::try_from(counted.min(i64::from(SEARCH_TOTAL_CAP))).unwrap_or(SEARCH_TOTAL_CAP);

        let selected: Vec<(i64, f64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT f.rowid, bm25(message_fts, 4.0, 3.0, 2.0, 1.0, 0.5, 1.0, 2.0)
                 FROM message_fts f
                 JOIN message_fts_rank r ON r.rowid = f.rowid
                 WHERE message_fts MATCH ?1
                 ORDER BY bm25(message_fts, 4.0, 3.0, 2.0, 1.0, 0.5, 1.0, 2.0) ASC,
                          r.date_key DESC, r.id_key ASC
                 LIMIT ?2 OFFSET ?3",
            )?;
            stmt.query_map(params![fts, i64::from(limit), i64::from(offset)], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if selected.is_empty() {
            return Ok(SearchPage { hits: Vec::new(), total, total_capped });
        }
        let rowids = selected.iter().map(|(rowid, _)| *rowid).collect::<Vec<_>>();
        let placeholders = std::iter::repeat_n("?", rowids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT m.rowid, {SUMMARY_COLUMNS}, substr(m.body_text, 1, {SNIPPET_SOURCE_CHARS})
             FROM messages m WHERE m.rowid IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut by_rowid = std::collections::HashMap::new();
        for row in
            stmt.query_map(params_from_iter(rowids.iter()), |row| Ok((row.get::<_, i64>(0)?, row_to_summary_at(row, 1)?, row.get::<_, String>(14)?)))?
        {
            let (rowid, summary, body_text) = row?;
            by_rowid.insert(rowid, (summary, body_text));
        }
        drop(stmt);
        let mut hits = selected
            .iter()
            .filter_map(|(rowid, rank)| {
                let (message, body_text) = by_rowid.remove(rowid)?;
                Some(SearchHit { message, body_text, rank: *rank })
            })
            .collect::<Vec<_>>();
        self.attach_labels(&mut hits)?;
        Ok(SearchPage { hits, total, total_capped })
    }

    fn attach_labels(&self, hits: &mut [SearchHit]) -> Result<(), StoreError> {
        let ids = hits.iter().map(|hit| hit.message.id.clone()).collect::<Vec<_>>();
        let labels = self.labels_by_message(&ids)?;
        for hit in hits {
            if let Some(ids) = labels.get(&hit.message.id) {
                hit.message.labels.clone_from(ids);
            }
        }
        Ok(())
    }

    /// Cheap readiness for the query path: the persisted state is ready and the counts agree. The
    /// full row-id drift check belongs to `search_status`, which is allowed to be slower.
    pub fn search_index_ready(&self) -> Result<bool, StoreError> {
        let (meta, damaged) = self.index_meta();
        if damaged.is_some() || meta.state == "error" {
            return Ok(false);
        }
        Ok(self.fts_documents()? == self.message_count()?)
    }

    /// Cheap progress for the shell: no full row-id drift scan. The full `search_status` remains the
    /// operation and is allowed to be slower.
    pub fn search_progress(&self) -> Result<SearchIndexStatus, StoreError> {
        let (meta, damaged) = self.index_meta();
        let indexed = self.fts_documents()?;
        let total = self.message_count()?;
        let counts_complete = indexed == total;
        let error_state = damaged.is_some() || meta.state == "error";
        let ready = !error_state && counts_complete;
        let state = if error_state {
            "error"
        } else if counts_complete {
            "ready"
        } else {
            meta.state.as_str()
        };
        Ok(SearchIndexStatus {
            state: state.to_owned(),
            indexed,
            total,
            ready,
            last_error: damaged.or_else(|| meta.last_error.clone()),
            size_bytes: self.database_size_bytes().unwrap_or(0),
            updated_at: self.search_updated_at()?.unwrap_or(meta.updated_at),
        })
    }

    /// Read the persisted index state and measure the live rows. A damaged value is reported as
    /// `error` with a readable sentence rather than failing the status operation.
    pub fn search_status(&self) -> Result<SearchIndexStatus, StoreError> {
        let (meta, damaged) = self.index_meta();
        let health = self.index_health()?;
        let drift = health.drift();
        let error_state = damaged.is_some();
        let counts_complete = !drift && health.indexed == health.total;
        let ready = !error_state && counts_complete && meta.state != "error";
        let state = if error_state {
            "error"
        } else if drift && meta.state == "ready" {
            "drift"
        } else if counts_complete && meta.state != "error" {
            "ready"
        } else {
            meta.state.as_str()
        };
        let last_error = damaged.or_else(|| meta.last_error.clone());
        let updated_at = self.search_updated_at()?.unwrap_or(meta.updated_at);
        Ok(SearchIndexStatus {
            state: state.to_owned(),
            indexed: health.indexed,
            total: health.total,
            ready,
            last_error,
            size_bytes: self.database_size_bytes().unwrap_or(0),
            updated_at,
        })
    }

    /// Finish or resume the whole index. The builder is idempotent: a batch transaction records
    /// `last_rowid`, so a crash resumes after the last committed 500-row batch. A ready index with no
    /// row-id drift is a no-op. This is the explicit maintenance path; `Store::open` never calls it.
    pub fn ensure_search_index(&self) -> Result<SearchReindexOutcome, StoreError> {
        self.ensure_search_index_inner(false, None)
    }

    /// Build at most one 500-row batch and return. The search operation calls this when the index is
    /// pending or building, so the UI can show progress instead of blocking open for a whole rebuild.
    pub fn ensure_search_index_batch(&self) -> Result<SearchReindexOutcome, StoreError> {
        self.ensure_search_index_inner(false, Some(1))
    }

    /// Delete every FTS/rank document and rebuild from the message rows. The one path that
    /// intentionally removes the whole index; it never runs `VACUUM`, because FTS keys on
    /// `messages.rowid` and a `VACUUM` would renumber rows.
    pub fn search_reindex(&self) -> Result<SearchReindexOutcome, StoreError> {
        self.ensure_search_index_inner(true, None)
    }

    fn ensure_search_index_inner(&self, force: bool, max_batches: Option<u32>) -> Result<SearchReindexOutcome, StoreError> {
        let started = Instant::now();
        let (mut meta, damaged) = self.index_meta();
        if force || damaged.is_some() {
            self.conn.execute_batch("DELETE FROM message_fts; DELETE FROM message_fts_rank;")?;
            meta = IndexMeta { state: "pending".to_owned(), last_rowid: 0, ..IndexMeta::default() };
            self.write_index_meta(&meta)?;
        } else if meta.state == "ready" {
            let health = self.index_health()?;
            if !health.drift() {
                return Ok(SearchReindexOutcome { documents: 0, took_ms: elapsed_ms(started) });
            }
            // Same-count drift or a missing/extra row: contentless FTS cannot repair individual rows
            // safely, so rebuild. `search_status` reports the drift before this runs.
            self.conn.execute_batch("DELETE FROM message_fts; DELETE FROM message_fts_rank;")?;
            meta = IndexMeta { state: "pending".to_owned(), last_rowid: 0, ..IndexMeta::default() };
            self.write_index_meta(&meta)?;
        }
        if meta.state != "building" && meta.state != "pending" {
            meta.state = "pending".to_owned();
        }
        meta.last_error = None;
        meta.updated_at = crate::now_ms();
        self.write_index_meta(&meta)?;

        let result = self.build_index_batches(&mut meta, max_batches);
        match result {
            Ok(true) => {
                meta.state = "ready".to_owned();
                meta.indexed = self.fts_documents()?;
                meta.total = self.message_count()?;
                meta.updated_at = crate::now_ms();
                self.write_index_meta(&meta)?;
                Ok(SearchReindexOutcome { documents: meta.indexed, took_ms: elapsed_ms(started) })
            }
            Ok(false) => {
                meta.state = "building".to_owned();
                meta.indexed = self.fts_documents()?;
                meta.total = self.message_count()?;
                meta.updated_at = crate::now_ms();
                self.write_index_meta(&meta)?;
                Ok(SearchReindexOutcome { documents: meta.indexed, took_ms: elapsed_ms(started) })
            }
            Err(error) => {
                meta.state = "error".to_owned();
                meta.last_error = Some(error.to_string());
                let _ = self.write_index_meta(&meta);
                Err(error)
            }
        }
    }

    /// One or more committed batches. Returns `true` when there is nothing left to index.
    fn build_index_batches(&self, meta: &mut IndexMeta, max_batches: Option<u32>) -> Result<bool, StoreError> {
        let mut batches = 0u32;
        loop {
            let tx = self.conn.unchecked_transaction()?;
            let mut stmt = tx.prepare(&format!(
                "SELECT m.rowid, m.subject, m.search_sender, m.search_to, m.search_cc, m.search_bcc,
                        substr(m.body_text, 1, {BODY_INDEX_CHARS}), m.attachment_names, m.date, m.id
                 FROM messages m
                 WHERE m.rowid > ?1
                   AND (NOT EXISTS(SELECT 1 FROM message_fts f WHERE f.rowid = m.rowid)
                        OR NOT EXISTS(SELECT 1 FROM message_fts_rank r WHERE r.rowid = m.rowid))
                 ORDER BY m.rowid
                 LIMIT {SEARCH_BATCH}"
            ))?;
            let rows = stmt
                .query_map([meta.last_rowid], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if rows.is_empty() {
                meta.state = "ready".to_owned();
                meta.indexed = self.fts_documents()?;
                meta.total = self.message_count()?;
                meta.updated_at = crate::now_ms();
                self.write_index_meta(meta)?;
                tx.commit()?;
                return Ok(true);
            }
            for row in &rows {
                tx.execute(
                    "INSERT INTO message_fts(rowid, subject, search_sender, search_to, search_cc, search_bcc, body_text, attachment_names)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7],
                )?;
                tx.execute(
                    "INSERT INTO message_fts_rank(rowid, date_key, id_key) VALUES(?1, ?2, ?3)
                     ON CONFLICT(rowid) DO UPDATE SET date_key = excluded.date_key, id_key = excluded.id_key",
                    params![row.0, row.8, row.9],
                )?;
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('search_updated_at', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [crate::now_ms().to_string()],
            )?;
            meta.last_rowid = rows.last().map_or(meta.last_rowid, |row| row.0);
            meta.state = "building".to_owned();
            meta.indexed = self.fts_documents()?;
            meta.total = self.message_count()?;
            meta.updated_at = crate::now_ms();
            self.write_index_meta(meta)?;
            tx.commit()?;
            batches = batches.saturating_add(1);
            if max_batches.is_some_and(|max| batches >= max) {
                return Ok(false);
            }
        }
    }

    fn index_health(&self) -> Result<IndexHealth, StoreError> {
        let indexed = self.fts_documents()?;
        let total = self.message_count()?;
        let missing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages m WHERE NOT EXISTS(SELECT 1 FROM message_fts f WHERE f.rowid = m.rowid)",
            [],
            |row| row.get(0),
        )?;
        let extra: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM message_fts f WHERE NOT EXISTS(SELECT 1 FROM messages m WHERE m.rowid = f.rowid)",
            [],
            |row| row.get(0),
        )?;
        let rank_missing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages m WHERE NOT EXISTS(SELECT 1 FROM message_fts_rank r WHERE r.rowid = m.rowid)",
            [],
            |row| row.get(0),
        )?;
        let rank_extra: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM message_fts_rank r WHERE NOT EXISTS(SELECT 1 FROM message_fts f WHERE f.rowid = r.rowid)",
            [],
            |row| row.get(0),
        )?;
        Ok(IndexHealth {
            indexed,
            total,
            missing: u32::try_from(missing).unwrap_or(u32::MAX),
            extra: u32::try_from(extra).unwrap_or(u32::MAX),
            rank_missing: u32::try_from(rank_missing).unwrap_or(u32::MAX),
            rank_extra: u32::try_from(rank_extra).unwrap_or(u32::MAX),
        })
    }

    fn search_updated_at(&self) -> Result<Option<i64>, StoreError> {
        Ok(self.get_meta(SEARCH_UPDATED_KEY)?.and_then(|value| value.parse::<i64>().ok()))
    }

    fn write_index_meta(&self, meta: &IndexMeta) -> Result<(), StoreError> {
        let value = serde_json::to_string(meta)?;
        self.set_meta(SEARCH_INDEX_KEY, &value)
    }

    fn index_meta(&self) -> (IndexMeta, Option<String>) {
        match self.get_meta(SEARCH_INDEX_KEY) {
            Ok(Some(raw)) => match serde_json::from_str::<IndexMeta>(&raw) {
                Ok(meta) => (meta, None),
                Err(error) => (
                    IndexMeta::default(),
                    Some(format!("the stored search index state is unreadable ({error}); run search_reindex to rebuild the index")),
                ),
            },
            Ok(None) => (IndexMeta::default(), None),
            Err(error) => (IndexMeta::default(), Some(format!("the stored search index state could not be read: {error}"))),
        }
    }

    fn fts_documents(&self) -> Result<u32, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM message_fts", [], |row| row.get(0))?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    fn message_count(&self) -> Result<u32, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    fn database_size_bytes(&self) -> Result<u64, StoreError> {
        let mut total = std::fs::metadata(&self.path).map(|metadata| metadata.len()).unwrap_or(0);
        for suffix in ["-wal", "-shm"] {
            let mut name = self.path.as_os_str().to_os_string();
            name.push(suffix);
            total = total.saturating_add(std::fs::metadata(std::path::Path::new(&name)).map(|metadata| metadata.len()).unwrap_or(0));
        }
        Ok(total)
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Compile a typed expression into fixed SQL fragments with bound values. The recursion depth is
/// bounded again here, because the store crate can be called directly by its own tests.
pub(crate) fn expr_sql(expr: &SearchExpr, values: &mut Vec<Value>, depth: &mut u8) -> Result<String, StoreError> {
    *depth = depth.saturating_add(1);
    if *depth > 16 {
        return Err(StoreError::Corrupt { table: "search", detail: "query nesting is deeper than 16 levels".to_owned() });
    }
    let sql = match expr {
        SearchExpr::And(children) => {
            if children.is_empty() {
                "1 = 1".to_owned()
            } else {
                let parts = children.iter().map(|child| expr_sql(child, values, depth)).collect::<Result<Vec<_>, _>>()?;
                format!("({})", parts.join(" AND "))
            }
        }
        SearchExpr::Or(children) => {
            if children.is_empty() {
                "1 = 0".to_owned()
            } else {
                let parts = children.iter().map(|child| expr_sql(child, values, depth)).collect::<Result<Vec<_>, _>>()?;
                format!("({})", parts.join(" OR "))
            }
        }
        SearchExpr::Not(child) => format!("NOT ({})", expr_sql(child, values, depth)?),
        SearchExpr::Match(expression) => {
            if expression.trim().is_empty() {
                return Err(StoreError::Corrupt { table: "search", detail: "an empty MATCH expression is not valid".to_owned() });
            }
            values.push(Value::Text(expression.clone()));
            "m.rowid IN (SELECT rowid FROM message_fts WHERE message_fts MATCH ?)".to_owned()
        }
        SearchExpr::Folder(id) => {
            values.push(Value::Text(id.clone()));
            "m.folder_id = ?".to_owned()
        }
        SearchExpr::Label(id) => {
            values.push(Value::Text(id.clone()));
            "EXISTS(SELECT 1 FROM message_labels ml WHERE ml.message_id = m.id AND ml.label_id = ?)".to_owned()
        }
        SearchExpr::Unread(flag) => {
            values.push(Value::Integer(i64::from(*flag)));
            "m.unread = ?".to_owned()
        }
        SearchExpr::Starred(flag) => {
            values.push(Value::Integer(i64::from(*flag)));
            "m.starred = ?".to_owned()
        }
        SearchExpr::HasAttachments(flag) => {
            values.push(Value::Integer(i64::from(*flag)));
            "m.has_attachments = ?".to_owned()
        }
        SearchExpr::After(instant) => {
            values.push(Value::Integer(*instant));
            "m.date >= ?".to_owned()
        }
        SearchExpr::Before(instant) => {
            values.push(Value::Integer(*instant));
            "m.date < ?".to_owned()
        }
        SearchExpr::Larger(bytes) => {
            values.push(Value::Integer(i64::try_from(*bytes).unwrap_or(i64::MAX)));
            "m.size > ?".to_owned()
        }
        SearchExpr::Smaller(bytes) => {
            values.push(Value::Integer(i64::try_from(*bytes).unwrap_or(i64::MAX)));
            "m.size < ?".to_owned()
        }
    };
    *depth = depth.saturating_sub(1);
    Ok(sql)
}

/// A boolean text expression that is valid as one FTS5 query. `None` when any leaf is a SQL filter
/// or a negation (FTS5 has no unary NOT), in which case the general typed path runs.
fn pure_fts_expression(expr: &SearchExpr) -> Option<String> {
    match expr {
        SearchExpr::Match(value) if !value.trim().is_empty() => Some(value.clone()),
        SearchExpr::Match(_) => None,
        SearchExpr::And(children) => {
            if children.is_empty() {
                return None;
            }
            let parts = children.iter().map(pure_fts_expression).collect::<Option<Vec<_>>>()?;
            Some(format!("({})", parts.join(" AND ")))
        }
        SearchExpr::Or(children) => {
            if children.is_empty() {
                return None;
            }
            let parts = children.iter().map(pure_fts_expression).collect::<Option<Vec<_>>>()?;
            Some(format!("({})", parts.join(" OR ")))
        }
        _ => None,
    }
}

/// Every positive text leaf joined with `OR`, for one bm25 computation. Text under `NOT` is
/// deliberately excluded: it is a filter, not a term the page is ranked by. `None` means the query
/// has no text, so the store skips `MATCH` entirely.
fn positive_match_expression(expr: &SearchExpr) -> Option<String> {
    fn collect(expr: &SearchExpr, out: &mut Vec<String>) {
        match expr {
            SearchExpr::And(children) | SearchExpr::Or(children) => {
                for child in children {
                    collect(child, out);
                }
            }
            SearchExpr::Not(_) => {}
            SearchExpr::Match(expression) => out.push(expression.clone()),
            _ => {}
        }
    }
    let mut leaves = Vec::new();
    collect(expr, &mut leaves);
    if leaves.is_empty() { None } else { Some(leaves.join(" OR ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leaf_expression_is_always_a_bound_value() {
        let mut values = Vec::new();
        let mut depth = 0;
        let sql = expr_sql(&SearchExpr::Match("\"fattura\"".to_owned()), &mut values, &mut depth).expect("sql");
        assert!(sql.contains("MATCH ?"));
        assert!(!sql.contains("fattura"));
        assert_eq!(values, vec![Value::Text("\"fattura\"".to_owned())]);
    }

    #[test]
    fn negative_only_text_has_no_rank_expression() {
        let expr = SearchExpr::Not(Box::new(SearchExpr::Match("\"spam\"".to_owned())));
        assert!(positive_match_expression(&expr).is_none());
    }
}
