//! 심볼 그래프 쿼리 — AI 에이전트가 소비하는 라인 기반 텍스트를 만든다.
//!
//! 에이전트 UX 원칙:
//! - `Class::Member` qualified name 을 1급 쿼리로 지원한다 (USR 패턴 매치).
//! - 이름이 모호하면 소수(AUTO_EXPAND_MAX 이하)는 후보별 결과를 바로 묶어 반환해
//!   재질의 왕복을 없애고, 다수면 USR 후보 목록을 안내한다.
//! - 결과가 limit 에 잘리면 "전체 N건 중 M건" 을 명시한다.
//! - 경로는 Shortener 로 축약해 토큰을 아낀다 (프로젝트 상대 / UE/ 접두).

use anyhow::Result;
use rusqlite::Connection;

use crate::ffi::{
    ROLE_ADDRESSOF, ROLE_CALL, ROLE_DECLARATION, ROLE_DEFINITION, ROLE_DYNAMIC, ROLE_IMPLICIT,
    ROLE_READ, ROLE_REFERENCE, ROLE_REL_BASEOF, ROLE_REL_CALLEDBY, ROLE_REL_CHILDOF,
    ROLE_REL_CONTAINEDBY, ROLE_REL_OVERRIDEOF, ROLE_REL_SPECIALIZATIONOF, ROLE_WRITE,
};

/// 이름이 이 수 이하로 모호하면 후보별 결과를 자동 확장한다.
const AUTO_EXPAND_MAX: usize = 5;

/// UE 리플렉션(UHT/GENERATED_BODY) 보일러플레이트 심볼을 거르는 SQL 조건.
/// members/outline 은 사용자가 작성한 API 를 보여주는 게 목적이라 기본 적용한다.
const UHT_NOISE_FILTER: &str = "
  AND s.name NOT IN ('StaticClass','StaticPackage','StaticClassFlags','StaticClassCastFlags',
    'StaticAllClassCastFlags','GetPrivateStaticClass','Super','ThisClass',
    'operator new','operator delete','WithinClass')
  AND s.name NOT LIKE 'exec%'
  AND s.name NOT LIKE 'StaticRegisterNatives%'
  AND s.name NOT LIKE 'Z\\_Construct%' ESCAPE '\\'
  AND s.name NOT LIKE '\\_\\_%' ESCAPE '\\'";

/// 결과 경로 축약기. (prefix → replacement) 를 앞에서부터 첫 매치로 적용한다.
#[derive(Default)]
pub struct Shortener {
    pairs: Vec<(String, String)>,
}

impl Shortener {
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self { pairs }
    }

    fn s(&self, path: &str) -> String {
        for (prefix, rep) in &self.pairs {
            if let Some(rest) = path.strip_prefix(prefix.as_str()) {
                return format!("{rep}{rest}");
            }
        }
        path.to_string()
    }
}

/// 심볼 kind 코드 → 표시 이름 (indexstore_symbol_kind_t).
fn kind_name(kind: i64) -> &'static str {
    match kind {
        1 => "module",
        2 => "namespace",
        3 => "namespace-alias",
        4 => "macro",
        5 => "enum",
        6 => "struct",
        7 => "class",
        8 => "protocol",
        9 => "extension",
        10 => "union",
        11 => "typealias",
        12 => "function",
        13 => "variable",
        14 => "field",
        15 => "enum-constant",
        16 => "instance-method",
        17 => "class-method",
        18 => "static-method",
        19 => "instance-property",
        20 => "class-property",
        21 => "static-property",
        22 => "constructor",
        23 => "destructor",
        24 => "conversion-function",
        25 => "parameter",
        26 => "using",
        27 => "concept",
        _ => "unknown",
    }
}

fn roles_text(roles: i64) -> String {
    let r = roles as u64;
    let mut tags = Vec::new();
    if r & ROLE_DEFINITION != 0 {
        tags.push("def");
    }
    if r & ROLE_DECLARATION != 0 {
        tags.push("decl");
    }
    if r & ROLE_CALL != 0 {
        tags.push("call");
    }
    if r & ROLE_REFERENCE != 0 {
        tags.push("ref");
    }
    if r & ROLE_READ != 0 {
        tags.push("read");
    }
    if r & ROLE_WRITE != 0 {
        tags.push("write");
    }
    if r & ROLE_ADDRESSOF != 0 {
        tags.push("addrof");
    }
    if r & ROLE_DYNAMIC != 0 {
        tags.push("dyn");
    }
    if r & ROLE_IMPLICIT != 0 {
        tags.push("implicit");
    }
    tags.join(",")
}

/// 매치된 심볼.
pub struct Sym {
    pub id: i64,
    pub usr: String,
    pub name: String,
    pub kind: i64,
}

fn row_sym(r: &rusqlite::Row) -> rusqlite::Result<Sym> {
    Ok(Sym {
        id: r.get(0)?,
        usr: r.get(1)?,
        name: r.get(2)?,
        kind: r.get(3)?,
    })
}

/// 이름/USR/qualified name 으로 심볼을 해석한다.
///
/// - "c:..." → USR 정확 매치
/// - "A::B::name" → simple name 정확 매치 + USR 세그먼트 순서 패턴("%@A@%@B@%@name%")
///   — USR 은 컨테이너를 "@S@A@S@B@F@name#시그니처" 로 인코딩하므로 순서 매치로 스코프를 좁힌다.
/// - 그 외 → 이름 정확 매치, 없으면 서브스트링 매치
pub fn resolve(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Sym>> {
    if query.starts_with("c:") {
        let mut stmt = conn.prepare_cached("SELECT id, usr, name, kind FROM symbols WHERE usr=?1")?;
        let syms = stmt.query_map([query], row_sym)?.collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(syms);
    }
    if query.contains("::") {
        let segs: Vec<&str> = query.split("::").filter(|s| !s.is_empty()).collect();
        if let Some((name, containers)) = segs.split_last() {
            let mut pat = String::from("%");
            for c in containers {
                pat.push_str(&format!("@{c}@%"));
            }
            pat.push_str(&format!("@{name}%"));
            let mut stmt = conn.prepare_cached(
                "SELECT id, usr, name, kind FROM symbols WHERE name=?1 AND usr LIKE ?2 LIMIT ?3",
            )?;
            let syms = stmt
                .query_map(rusqlite::params![name, pat, limit as i64], row_sym)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(syms);
        }
    }
    let mut stmt =
        conn.prepare_cached("SELECT id, usr, name, kind FROM symbols WHERE name=?1 LIMIT ?2")?;
    let exact = stmt
        .query_map(rusqlite::params![query, limit as i64], row_sym)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !exact.is_empty() {
        return Ok(exact);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT id, usr, name, kind FROM symbols WHERE name LIKE ?1 ORDER BY length(name) LIMIT ?2",
    )?;
    let subs = stmt
        .query_map(rusqlite::params![format!("%{query}%"), limit as i64], row_sym)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(subs)
}

/// 심볼의 정의 위치("path:line") 목록. 같은 파일이 컴파일 컨텍스트별 record 를
/// 여럿 가질 수 있어 DISTINCT 가 필수다.
fn def_locations(conn: &Connection, sh: &Shortener, sym_id: i64, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT f.path, o.line FROM occurrences o
         JOIN records r ON r.id = o.record_id
         JOIN files f ON f.id = r.file_id
         WHERE o.symbol_id = ?1 AND (o.roles & ?2) != 0
         ORDER BY f.path LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![sym_id, ROLE_DEFINITION as i64, limit as i64],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(|(p, l)| format!("{}:{}", sh.s(&p), l)).collect())
}

fn sym_headline(conn: &Connection, sh: &Shortener, s: &Sym) -> Result<String> {
    let defs = def_locations(conn, sh, s.id, 3)?;
    let loc = if defs.is_empty() {
        "(no definition indexed)".to_string()
    } else {
        defs.join(" | ")
    };
    Ok(format!("{} [{}] {}\n  usr: {}", s.name, kind_name(s.kind), loc, s.usr))
}

/// 심볼 검색: 이름 매치 + kind + 정의 위치 + USR.
pub fn find(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let syms = resolve(conn, query, limit)?;
    if syms.is_empty() {
        return Ok(format!("no symbols match: {query}"));
    }
    let mut out = String::new();
    for s in &syms {
        out.push_str(&sym_headline(conn, sh, s)?);
        out.push('\n');
    }
    Ok(out)
}

/// 대상 심볼 해석 + 자동 확장 정책.
/// AUTO_EXPAND_MAX 이하의 모호함은 전체 후보를 반환해 호출측이 각각 질의하고,
/// 그보다 많으면 후보 목록 문자열을 반환한다.
fn resolve_targets(conn: &Connection, sh: &Shortener, query: &str) -> Result<std::result::Result<Vec<Sym>, String>> {
    let syms = resolve(conn, query, 30)?;
    if syms.is_empty() {
        return Ok(Err(format!("no symbols match: {query}")));
    }
    if syms.len() > AUTO_EXPAND_MAX {
        let mut msg = format!(
            "ambiguous ({} matches, showing 30 max) — narrow with Class::Member or a USR:\n",
            syms.len()
        );
        for s in &syms {
            msg.push_str("  ");
            msg.push_str(&sym_headline(conn, sh, s)?.replace("\n  usr:", "  usr:"));
            msg.push('\n');
        }
        return Ok(Err(msg));
    }
    Ok(Ok(syms))
}

/// (path, line, col, roles) 행들을 "전체 N건 중 M건" 헤더와 함께 포맷한다.
struct OccRows {
    total: i64,
    lines: Vec<String>,
}

impl OccRows {
    fn render(&self, out: &mut String) {
        if self.total as usize > self.lines.len() {
            out.push_str(&format!("  ({} total, showing {})\n", self.total, self.lines.len()));
        }
        for l in &self.lines {
            out.push_str("  ");
            out.push_str(l);
            out.push('\n');
        }
    }
}

/// 모든 occurrence (정의/선언/참조/호출) 나열.
pub fn refs(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let syms = match resolve_targets(conn, sh, query)? {
        Ok(s) => s,
        Err(msg) => return Ok(msg),
    };
    let mut out = String::new();
    for sym in &syms {
        let total: i64 = conn
            .prepare_cached("SELECT COUNT(DISTINCT r.file_id || ':' || o.line || ':' || o.col) FROM occurrences o JOIN records r ON r.id=o.record_id WHERE o.symbol_id=?1")?
            .query_row([sym.id], |r| r.get(0))?;
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT f.path, o.line, o.col, o.roles FROM occurrences o
             JOIN records r ON r.id = o.record_id
             JOIN files f ON f.id = r.file_id
             WHERE o.symbol_id = ?1
             ORDER BY f.path, o.line LIMIT ?2",
        )?;
        let lines = stmt
            .query_map(rusqlite::params![sym.id, limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(p, l, c, ro)| format!("{}:{}:{} [{}]", sh.s(&p), l, c, roles_text(ro)))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "{} [{}] usr: {}\noccurrences:\n",
            sym.name,
            kind_name(sym.kind),
            sym.usr
        ));
        OccRows { total, lines }.render(&mut out);
    }
    Ok(out)
}

/// caller/callee 공용 행 포맷.
fn related_rows(
    conn: &Connection,
    sh: &Shortener,
    sql_count: &str,
    sql: &str,
    sym_id: i64,
    rel_mask: i64,
    limit: usize,
) -> Result<OccRows> {
    let total: i64 = conn
        .prepare_cached(sql_count)?
        .query_row(rusqlite::params![sym_id, rel_mask], |r| r.get(0))?;
    let mut stmt = conn.prepare_cached(sql)?;
    let lines = stmt
        .query_map(rusqlite::params![sym_id, rel_mask, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(name, kind, usr, path, line, roles)| {
            format!(
                "{} [{}] at {}:{} [{}]  usr: {}",
                name,
                kind_name(kind),
                sh.s(&path),
                line,
                roles_text(roles),
                usr
            )
        })
        .collect::<Vec<_>>();
    Ok(OccRows { total, lines })
}

/// 이 심볼을 호출/참조하는 곳 — occurrence 의 CALLEDBY/CONTAINEDBY relation 이 컨테이너다.
pub fn callers(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let syms = match resolve_targets(conn, sh, query)? {
        Ok(s) => s,
        Err(msg) => return Ok(msg),
    };
    let rel_mask = (ROLE_REL_CALLEDBY | ROLE_REL_CONTAINEDBY) as i64;
    let mut out = String::new();
    for sym in &syms {
        let rows = related_rows(
            conn,
            sh,
            "SELECT COUNT(DISTINCT s.usr || '@' || f.path || ':' || o.line) FROM occurrences o
             JOIN relations rel ON rel.occ_id = o.id
             JOIN symbols s ON s.id = rel.symbol_id
             JOIN records r ON r.id = o.record_id
             JOIN files f ON f.id = r.file_id
             WHERE o.symbol_id = ?1 AND (rel.roles & ?2) != 0",
            "SELECT DISTINCT s.name, s.kind, s.usr, f.path, o.line, o.roles FROM occurrences o
             JOIN relations rel ON rel.occ_id = o.id
             JOIN symbols s ON s.id = rel.symbol_id
             JOIN records r ON r.id = o.record_id
             JOIN files f ON f.id = r.file_id
             WHERE o.symbol_id = ?1 AND (rel.roles & ?2) != 0
             ORDER BY (o.roles & 32) DESC, f.path, o.line LIMIT ?3",
            sym.id,
            rel_mask,
            limit,
        )?;
        out.push_str(&format!(
            "callers/referencers of {} ({}) — usr: {}\n",
            sym.name, rows.total, sym.usr
        ));
        rows.render(&mut out);
    }
    Ok(out)
}

/// 이 심볼(함수) 본문이 호출/참조하는 대상들. 호출(call)을 앞세워 정렬한다.
pub fn callees(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let syms = match resolve_targets(conn, sh, query)? {
        Ok(s) => s,
        Err(msg) => return Ok(msg),
    };
    let rel_mask = (ROLE_REL_CALLEDBY | ROLE_REL_CONTAINEDBY) as i64;
    let mut out = String::new();
    for sym in &syms {
        let rows = related_rows(
            conn,
            sh,
            "SELECT COUNT(DISTINCT s.usr || '@' || f.path || ':' || o.line) FROM relations rel
             JOIN occurrences o ON o.id = rel.occ_id
             JOIN symbols s ON s.id = o.symbol_id
             JOIN records r ON r.id = o.record_id
             JOIN files f ON f.id = r.file_id
             WHERE rel.symbol_id = ?1 AND (rel.roles & ?2) != 0",
            "SELECT DISTINCT s.name, s.kind, s.usr, f.path, o.line, o.roles FROM relations rel
             JOIN occurrences o ON o.id = rel.occ_id
             JOIN symbols s ON s.id = o.symbol_id
             JOIN records r ON r.id = o.record_id
             JOIN files f ON f.id = r.file_id
             WHERE rel.symbol_id = ?1 AND (rel.roles & ?2) != 0
             ORDER BY (o.roles & 32) DESC, f.path, o.line LIMIT ?3",
            sym.id,
            rel_mask,
            limit,
        )?;
        out.push_str(&format!(
            "callees/uses inside {} ({}) — usr: {}\n",
            sym.name, rows.total, sym.usr
        ));
        rows.render(&mut out);
    }
    Ok(out)
}

/// 상속·오버라이드 계층: bases / derived / overrides / overridden-by.
pub fn hierarchy(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let syms = match resolve_targets(conn, sh, query)? {
        Ok(s) => s,
        Err(msg) => return Ok(msg),
    };
    let mut out = String::new();
    for sym in &syms {
        // "타 심볼의 occurrence 가 (rel_role, X) 를 가질 때" 그 occurrence 심볼 쪽 (bases / overridden-by).
        let occ_side = |rel_role: u64| -> Result<Vec<String>> {
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT s.name, s.kind, s.usr FROM occurrences o
                 JOIN relations rel ON rel.occ_id = o.id
                 JOIN symbols s ON s.id = o.symbol_id
                 WHERE rel.symbol_id = ?1 AND (rel.roles & ?2) != 0 LIMIT ?3",
            )?;
            let v = stmt
                .query_map(rusqlite::params![sym.id, rel_role as i64, limit as i64], |r| {
                    Ok(format!(
                        "{} [{}] usr: {}",
                        r.get::<_, String>(0)?,
                        kind_name(r.get(1)?),
                        r.get::<_, String>(2)?
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        };
        // "X 의 occurrence 에 (rel_role, target) 이 붙을 때" target 쪽 (derived / overrides).
        let rel_side = |rel_role: u64| -> Result<Vec<String>> {
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT s.name, s.kind, s.usr FROM occurrences o
                 JOIN relations rel ON rel.occ_id = o.id
                 JOIN symbols s ON s.id = rel.symbol_id
                 WHERE o.symbol_id = ?1 AND (rel.roles & ?2) != 0 LIMIT ?3",
            )?;
            let v = stmt
                .query_map(rusqlite::params![sym.id, rel_role as i64, limit as i64], |r| {
                    Ok(format!(
                        "{} [{}] usr: {}",
                        r.get::<_, String>(0)?,
                        kind_name(r.get(1)?),
                        r.get::<_, String>(2)?
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        };

        out.push_str(&format!("{} [{}] usr: {}\n", sym.name, kind_name(sym.kind), sym.usr));
        let sections: [(&str, Vec<String>); 5] = [
            ("bases", occ_side(ROLE_REL_BASEOF)?),
            ("derived", rel_side(ROLE_REL_BASEOF)?),
            ("overrides (this overrides)", rel_side(ROLE_REL_OVERRIDEOF)?),
            ("overridden-by", occ_side(ROLE_REL_OVERRIDEOF)?),
            ("specializations", occ_side(ROLE_REL_SPECIALIZATIONOF)?),
        ];
        for (title, items) in sections {
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("{title} ({}):\n", items.len()));
            for l in items {
                out.push_str("  ");
                out.push_str(&l);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// 클래스/구조체/enum 의 멤버 일람 — CHILDOF relation 기반.
/// 세션 히스토리에서 `^void UStageDirectorSubsystem::` 류 grep 으로 반복되던 탐색의 대체.
pub fn members(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let mut syms = match resolve_targets(conn, sh, query)? {
        Ok(s) => s,
        Err(msg) => return Ok(msg),
    };
    // 같은 이름의 생성자 등이 섞이면 컨테이너 kind(namespace/enum/struct/class/protocol/union)만 남긴다.
    let is_container = |k: i64| matches!(k, 2 | 5 | 6 | 7 | 8 | 10);
    if syms.iter().any(|s| is_container(s.kind)) {
        syms.retain(|s| is_container(s.kind));
    }
    let mut out = String::new();
    for sym in &syms {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT DISTINCT s.id, s.name, s.kind, s.usr FROM occurrences o
             JOIN relations rel ON rel.occ_id = o.id
             JOIN symbols s ON s.id = o.symbol_id
             WHERE rel.symbol_id = ?1 AND (rel.roles & ?2) != 0
             {UHT_NOISE_FILTER}
             LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![sym.id, ROLE_REL_CHILDOF as i64, limit as i64],
                |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push_str(&format!(
            "members of {} [{}] ({}) — usr: {}\n",
            sym.name,
            kind_name(sym.kind),
            rows.len(),
            sym.usr
        ));
        // (파일, 줄) 기준 정렬로 헤더의 선언 순서를 재현한다.
        let mut items: Vec<(String, i64, String)> = Vec::new();
        for (id, name, kind, usr) in rows {
            let defs = def_locations(conn, sh, id, 1)?;
            let loc = defs.into_iter().next().unwrap_or_else(|| "(decl only)".to_string());
            let (path, line) = match loc.rsplit_once(':') {
                Some((p, l)) => (p.to_string(), l.parse::<i64>().unwrap_or(0)),
                None => (loc.clone(), 0),
            };
            items.push((format!("{name} [{}] {loc}  usr: {usr}", kind_name(kind)), line, path));
        }
        items.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
        for (line, _, _) in items {
            out.push_str("  ");
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// 파일 아웃라인 — 해당 파일에서 선언/정의되는 심볼을 줄 순서로 나열한다.
/// `query` 는 경로 서브스트링 (예: "MonsterBase.h").
pub fn outline(conn: &Connection, sh: &Shortener, query: &str, limit: usize) -> Result<String> {
    let mut stmt =
        conn.prepare_cached("SELECT id, path FROM files WHERE path LIKE ?1 ORDER BY length(path) LIMIT 4")?;
    let files = stmt
        .query_map([format!("%{query}%")], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if files.is_empty() {
        return Ok(format!("no indexed file matches: {query}"));
    }
    let mut out = String::new();
    for (file_id, path) in &files {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT DISTINCT s.name, s.kind, s.usr, MIN(o.line) FROM occurrences o
             JOIN records r ON r.id = o.record_id
             JOIN symbols s ON s.id = o.symbol_id
             WHERE r.file_id = ?1 AND (o.roles & ?2) != 0 AND (o.roles & ?3) = 0
               AND s.kind != 25
             {UHT_NOISE_FILTER}
             GROUP BY s.id ORDER BY MIN(o.line) LIMIT ?4"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    file_id,
                    (ROLE_DEFINITION | ROLE_DECLARATION) as i64,
                    ROLE_IMPLICIT as i64,
                    limit as i64
                ],
                |r| {
                    Ok(format!(
                        "{}: {} [{}]  usr: {}",
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(0)?,
                        kind_name(r.get(1)?),
                        r.get::<_, String>(2)?
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push_str(&format!("outline of {} ({} symbols):\n", sh.s(path), rows.len()));
        for l in rows {
            out.push_str("  ");
            out.push_str(&l);
            out.push('\n');
        }
    }
    Ok(out)
}

/// 그래프 현황 요약 (진단·헬스체크용).
pub fn stats(conn: &Connection) -> Result<String> {
    let one = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    Ok(format!(
        "units: {}\nrecords: {}\nfiles: {}\nsymbols: {}\noccurrences: {}\nrelations: {}",
        one("SELECT COUNT(*) FROM units")?,
        one("SELECT COUNT(*) FROM records")?,
        one("SELECT COUNT(*) FROM files")?,
        one("SELECT COUNT(*) FROM symbols")?,
        one("SELECT COUNT(*) FROM occurrences")?,
        one("SELECT COUNT(*) FROM relations")?,
    ))
}
