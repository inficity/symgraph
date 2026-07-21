//! 스토어 → SQLite 증분 머지.
//!
//! 속도 전략 (증분 갱신이 이 시스템의 1급 요구사항):
//! 1. unit 목록은 FFI 가 아니라 fs 디렉터리 스캔으로 얻는다 (파일명 + mtime).
//! 2. mtime 이 같은 unit 은 아예 열지 않는다. 빌드가 재컴파일한 TU 만 mtime 이 바뀐다.
//! 3. record 는 content-addressed — DB 에 이름이 있으면 재파싱하지 않고 링크만 건다.
//! 4. 전체 작업을 단일 트랜잭션 + prepared statement 캐시로 처리한다.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::db;
use crate::ffi::{IndexStoreLib, Store};

/// 머지 결과 요약.
#[derive(Debug, Default)]
pub struct MergeStats {
    pub units_total: usize,
    pub units_processed: usize,
    pub units_removed: usize,
    pub records_ingested: usize,
    pub records_reused: usize,
    pub occurrences_added: u64,
    pub elapsed_ms: u128,
}

impl MergeStats {
    pub fn summary(&self) -> String {
        format!(
            "units {}/{} processed, {} removed; records {} new / {} reused; {} occurrences; {} ms",
            self.units_processed,
            self.units_total,
            self.units_removed,
            self.records_ingested,
            self.records_reused,
            self.occurrences_added,
            self.elapsed_ms
        )
    }
}

/// 스토어를 DB 로 증분 머지한다. `full` 이면 DB 를 비우고 처음부터 다시 만든다.
pub fn merge(store_dir: &Path, db_path: &Path, full: bool) -> Result<MergeStats> {
    let t0 = Instant::now();
    let lib = IndexStoreLib::load()?;
    let store = lib.open_store(store_dir)?;
    let mut conn = db::open(db_path)?;
    if full {
        db::wipe(&conn)?;
    }

    let fs_units = scan_units_dir(store_dir)?;
    let mut stats = MergeStats {
        units_total: fs_units.len(),
        ..Default::default()
    };

    // DB 에 있는 unit 목록 (name → (id, mtime_ns, main_file_path))
    let mut db_units: HashMap<String, (i64, i64, Option<String>)> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT u.name, u.id, u.mtime_ns, f.path FROM units u
             LEFT JOIN files f ON f.id = u.main_file_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get::<_, Option<String>>(3)?)))
        })?;
        for row in rows {
            let (name, v) = row?;
            db_units.insert(name, v);
        }
    }

    // 처리 대상: fs 에 있고 (DB 에 없거나 mtime 이 다른) unit.
    // 제거 대상: DB 에 있는데 fs 에 없는 unit + main file 이 사라진 unit(소스 삭제/리네임 잔재).
    let mut to_process: Vec<&String> = Vec::new();
    for (name, mtime) in &fs_units {
        match db_units.get(name) {
            Some((_, db_mtime, _)) if db_mtime == mtime => {}
            _ => to_process.push(name),
        }
    }
    let mut to_remove: Vec<i64> = Vec::new();
    for (name, (id, _, main_file)) in &db_units {
        let gone_from_store = !fs_units.contains_key(name);
        let source_gone = main_file.as_ref().is_some_and(|p| !Path::new(p).exists());
        if gone_from_store || source_gone {
            // mtime 동일해도 소스가 사라졌으면 제거. (스토어는 스스로 unit 을 지우지 않는다)
            if source_gone && !gone_from_store {
                to_process.retain(|n| *n != name);
            }
            to_remove.push(*id);
        }
    }

    let tx = conn.transaction()?;
    {
        let mut m = Merger::new(&tx, &store)?;
        for id in &to_remove {
            m.remove_unit(*id)?;
            stats.units_removed += 1;
        }
        for name in to_process {
            let mtime = fs_units[name];
            m.process_unit(name, mtime, &mut stats)
                .with_context(|| format!("unit 처리: {name}"))?;
            stats.units_processed += 1;
        }
        if stats.units_removed > 0 || stats.units_processed > 0 {
            m.gc()?;
        }
    }
    tx.commit()?;

    stats.elapsed_ms = t0.elapsed().as_millis();
    Ok(stats)
}

/// `<store>/v5/units` 를 스캔해 unit 이름 → mtime(ns) 맵을 만든다.
fn scan_units_dir(store_dir: &Path) -> Result<HashMap<String, i64>> {
    let units_dir = store_dir.join("v5").join("units");
    let mut out = HashMap::new();
    let rd = std::fs::read_dir(&units_dir)
        .with_context(|| format!("units 디렉터리 없음 (스토어 미생성?): {}", units_dir.display()))?;
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let mtime_ns = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        out.insert(name, mtime_ns);
    }
    Ok(out)
}

/// 트랜잭션 스코프 안에서 unit/record 를 밀어넣는 작업자.
struct Merger<'a> {
    tx: &'a Connection,
    store: &'a Store<'a>,
    /// record name → id (DB 전체 프리로드 — 재사용 record 를 즉시 판별)
    record_ids: HashMap<String, i64>,
    /// usr → symbol id (머지 세션 캐시)
    symbol_ids: HashMap<String, i64>,
    /// path → file id (머지 세션 캐시)
    file_ids: HashMap<String, i64>,
}

impl<'a> Merger<'a> {
    fn new(tx: &'a Connection, store: &'a Store<'a>) -> Result<Self> {
        let mut record_ids = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT name, id FROM records")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (name, id) = row?;
                record_ids.insert(name, id);
            }
        }
        Ok(Self {
            tx,
            store,
            record_ids,
            symbol_ids: HashMap::new(),
            file_ids: HashMap::new(),
        })
    }

    fn file_id(&mut self, path: &str) -> Result<i64> {
        if let Some(id) = self.file_ids.get(path) {
            return Ok(*id);
        }
        let id: i64 = self
            .tx
            .prepare_cached("INSERT INTO files(path) VALUES(?1) ON CONFLICT(path) DO UPDATE SET path=path RETURNING id")?
            .query_row([path], |r| r.get(0))?;
        self.file_ids.insert(path.to_string(), id);
        Ok(id)
    }

    fn symbol_id(&mut self, usr: &str, name: &str, kind: u32, lang: u32) -> Result<i64> {
        if let Some(id) = self.symbol_ids.get(usr) {
            return Ok(*id);
        }
        let id: i64 = self
            .tx
            .prepare_cached(
                "INSERT INTO symbols(usr, name, kind, lang) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(usr) DO UPDATE SET usr=usr RETURNING id",
            )?
            .query_row(rusqlite::params![usr, name, kind, lang], |r| r.get(0))?;
        self.symbol_ids.insert(usr.to_string(), id);
        Ok(id)
    }

    /// unit 하나를 읽어 DB 에 반영한다. 이미 있던 unit 이면 행을 갈아끼운다.
    fn process_unit(&mut self, unit_name: &str, mtime_ns: i64, stats: &mut MergeStats) -> Result<()> {
        // 기존 행 제거 (재컴파일된 TU) — record 링크만 지우면 GC 가 고아 record 를 정리한다.
        if let Some(old_id) = self
            .tx
            .prepare_cached("SELECT id FROM units WHERE name=?1")?
            .query_row([unit_name], |r| r.get::<_, i64>(0))
            .ok()
        {
            self.remove_unit(old_id)?;
        }

        let reader = self.store.unit_reader(unit_name)?;
        let main_file_id = match reader.main_file() {
            Some(p) if !p.is_empty() => Some(self.file_id(&p)?),
            _ => None,
        };
        let module_name = reader.module_name();
        self.tx
            .prepare_cached("INSERT INTO units(name, main_file_id, module_name, mtime_ns) VALUES(?1, ?2, ?3, ?4)")?
            .execute(rusqlite::params![unit_name, main_file_id, module_name, mtime_ns])?;
        let unit_id = self.tx.last_insert_rowid();

        let mut seen: HashSet<i64> = HashSet::new();
        for dep in reader.record_deps() {
            let record_id = match self.record_ids.get(&dep.name) {
                Some(id) => {
                    stats.records_reused += 1;
                    *id
                }
                None => {
                    let id = self
                        .ingest_record(&dep.name, &dep.file_path, stats)
                        .with_context(|| format!("record 수집: {} ({})", dep.name, dep.file_path))?;
                    self.record_ids.insert(dep.name.clone(), id);
                    stats.records_ingested += 1;
                    id
                }
            };
            if seen.insert(record_id) {
                self.tx
                    .prepare_cached("INSERT OR IGNORE INTO unit_records(unit_id, record_id) VALUES(?1, ?2)")?
                    .execute([unit_id, record_id])?;
            }
        }
        Ok(())
    }

    /// record 파일을 파싱해 symbols/occurrences/relations 를 적재한다.
    fn ingest_record(&mut self, name: &str, file_path: &str, stats: &mut MergeStats) -> Result<i64> {
        let file_id = self.file_id(file_path)?;
        self.tx
            .prepare_cached("INSERT INTO records(name, file_id) VALUES(?1, ?2)")?
            .execute(rusqlite::params![name, file_id])?;
        let record_id = self.tx.last_insert_rowid();

        let reader = self.store.record_reader(name)?;
        let mut occs = Vec::new();
        reader.for_each_occurrence(|occ| occs.push(occ));

        for occ in occs {
            let sym_id = self.symbol_id(&occ.symbol.usr, &occ.symbol.name, occ.symbol.kind, occ.symbol.lang)?;
            self.tx
                .prepare_cached(
                    "INSERT INTO occurrences(record_id, symbol_id, roles, line, col) VALUES(?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(rusqlite::params![record_id, sym_id, occ.roles as i64, occ.line, occ.col])?;
            let occ_id = self.tx.last_insert_rowid();
            stats.occurrences_added += 1;
            for (roles, rel_sym) in occ.relations {
                let rel_sym_id = self.symbol_id(&rel_sym.usr, &rel_sym.name, rel_sym.kind, rel_sym.lang)?;
                self.tx
                    .prepare_cached("INSERT INTO relations(occ_id, roles, symbol_id) VALUES(?1, ?2, ?3)")?
                    .execute(rusqlite::params![occ_id, roles as i64, rel_sym_id])?;
            }
        }
        Ok(record_id)
    }

    fn remove_unit(&mut self, unit_id: i64) -> Result<()> {
        self.tx
            .prepare_cached("DELETE FROM unit_records WHERE unit_id=?1")?
            .execute([unit_id])?;
        self.tx.prepare_cached("DELETE FROM units WHERE id=?1")?.execute([unit_id])?;
        Ok(())
    }

    /// 어느 unit 도 참조하지 않는 record 와, 그에 딸린 occurrence/relation,
    /// 어디서도 참조되지 않는 symbol 을 정리한다.
    fn gc(&mut self) -> Result<()> {
        self.tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS dead_records(id INTEGER PRIMARY KEY);
             DELETE FROM dead_records;
             INSERT INTO dead_records
               SELECT r.id FROM records r
               WHERE NOT EXISTS(SELECT 1 FROM unit_records ur WHERE ur.record_id = r.id);
             DELETE FROM relations WHERE occ_id IN (
               SELECT o.id FROM occurrences o JOIN dead_records d ON o.record_id = d.id);
             DELETE FROM occurrences WHERE record_id IN (SELECT id FROM dead_records);
             DELETE FROM records WHERE id IN (SELECT id FROM dead_records);",
        )?;
        let removed_records = self.tx.changes();
        // record 가 하나라도 죽었을 때만 symbol GC (전체 심볼 스캔이라 조건부 실행)
        if removed_records > 0 {
            self.tx.execute_batch(
                "DELETE FROM symbols WHERE
                   NOT EXISTS(SELECT 1 FROM occurrences o WHERE o.symbol_id = symbols.id)
                   AND NOT EXISTS(SELECT 1 FROM relations r WHERE r.symbol_id = symbols.id);",
            )?;
            // record_ids 캐시에서 죽은 record 제거
            let mut alive: HashSet<String> = HashSet::new();
            let mut stmt = self.tx.prepare("SELECT name FROM records")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                alive.insert(row?);
            }
            self.record_ids.retain(|name, _| alive.contains(name));
        }
        self.tx.execute_batch("DELETE FROM dead_records;")?;
        Ok(())
    }
}
