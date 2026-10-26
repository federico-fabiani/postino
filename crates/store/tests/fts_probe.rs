//! Throwaway pre-flight probe for the P9a FTS5 decision (kept as the capability regression).

use rusqlite::Connection;

#[test]
fn bundled_sqlite_supports_contentless_delete() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    let version: String = conn.query_row("SELECT sqlite_version()", [], |row| row.get(0)).expect("sqlite version");
    eprintln!("SQLITE_VERSION={version}");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE probe USING fts5(a, b, content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2', prefix='2 3');
         INSERT INTO probe(rowid, a, b) VALUES (1, 'Città', 'fattura 2026'), (2, 'other', 'x');
         DELETE FROM probe WHERE rowid = 2;
         DELETE FROM probe WHERE rowid = 1;
         INSERT INTO probe(rowid, a, b) VALUES (1, 'Roma', 'fattura 2026');",
    )
    .expect("contentless_delete accepted");
    let count: i64 = conn.query_row("SELECT count(*) FROM probe", [], |row| row.get(0)).expect("count");
    assert_eq!(count, 1);
    let hit: i64 = conn.query_row("SELECT rowid FROM probe WHERE probe MATCH '\"roma\"'", [], |row| row.get(0)).expect("match after delete+insert");
    assert_eq!(hit, 1);
    let rowids: Vec<i64> = conn
        .prepare("SELECT rowid FROM probe ORDER BY rowid")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("scan")
        .collect::<rusqlite::Result<_>>()
        .expect("rowids");
    assert_eq!(rowids, vec![1]);
}
