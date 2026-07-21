//! 심볼 그래프 SQLite 스키마.
//!
//! 증분 머지의 기준 키:
//! - unit: 스토어 unit 파일명 + mtime (빌드가 재컴파일한 TU만 mtime 이 바뀐다)
//! - record: content-addressed 이름 — 같은 이름이면 내용 동일이라 재파싱하지 않는다
//! - symbol: USR 유일

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols(
  id INTEGER PRIMARY KEY,
  usr TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL,
  kind INTEGER NOT NULL,
  lang INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE TABLE IF NOT EXISTS records(
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  file_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_records_file ON records(file_id);
CREATE TABLE IF NOT EXISTS units(
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  main_file_id INTEGER,
  module_name TEXT,
  mtime_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS unit_records(
  unit_id INTEGER NOT NULL,
  record_id INTEGER NOT NULL,
  PRIMARY KEY(unit_id, record_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_unit_records_record ON unit_records(record_id);
CREATE TABLE IF NOT EXISTS occurrences(
  id INTEGER PRIMARY KEY,
  record_id INTEGER NOT NULL,
  symbol_id INTEGER NOT NULL,
  roles INTEGER NOT NULL,
  line INTEGER NOT NULL,
  col INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_occ_symbol ON occurrences(symbol_id);
CREATE INDEX IF NOT EXISTS idx_occ_record ON occurrences(record_id);
CREATE TABLE IF NOT EXISTS relations(
  occ_id INTEGER NOT NULL,
  roles INTEGER NOT NULL,
  symbol_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rel_occ ON relations(occ_id);
CREATE INDEX IF NOT EXISTS idx_rel_symbol ON relations(symbol_id);
";

/// DB 를 열고 스키마를 보장한다. 스키마 버전이 다르면 통째로 재생성한다
/// — 그래프는 스토어에서 언제든 재구축 가능한 파생 데이터라 마이그레이션하지 않는다.
pub fn open(db_path: &Path) -> Result<Connection> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("DB 디렉터리 생성: {}", dir.display()))?;
    }
    let conn = Connection::open(db_path).with_context(|| format!("DB 열기: {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -65536)?; // 64 MiB 페이지 캐시

    let version: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.parse().ok());
    if version != Some(SCHEMA_VERSION) {
        wipe(&conn)?;
    }
    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(conn)
}

/// 모든 그래프 데이터를 비운다 (full 리인덱스 / 스키마 변경 시).
pub fn wipe(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS meta;
         DROP TABLE IF EXISTS files;
         DROP TABLE IF EXISTS symbols;
         DROP TABLE IF EXISTS records;
         DROP TABLE IF EXISTS units;
         DROP TABLE IF EXISTS unit_records;
         DROP TABLE IF EXISTS occurrences;
         DROP TABLE IF EXISTS relations;",
    )?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}
