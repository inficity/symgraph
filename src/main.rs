//! symgraph CLI — 빌드 스텝/수동 실행용 얇은 진입점.
//!
//! usage:
//!   symgraph update <store-dir> <db-path> [--full]
//!   symgraph stats  <db-path>
//!   symgraph find|refs|callers|callees|hierarchy <db-path> <query> [limit]

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("symgraph error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage:\n  symgraph update <store-dir> <db-path> [--full]\n  symgraph stats <db-path>\n  symgraph find|refs|callers|callees|hierarchy|members|outline <db-path> <query> [limit]";
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "update" => {
            let store = args.get(1).context(usage)?;
            let db = args.get(2).context(usage)?;
            let full = args.iter().any(|a| a == "--full");
            let stats = symgraph::merge(Path::new(store), Path::new(db), full)?;
            Ok(stats.summary())
        }
        "stats" => {
            let db = args.get(1).context(usage)?;
            let conn = symgraph::db::open(Path::new(db))?;
            symgraph::query::stats(&conn)
        }
        "find" | "refs" | "callers" | "callees" | "hierarchy" | "members" | "outline" => {
            let db = args.get(1).context(usage)?;
            let q = args.get(2).context(usage)?;
            let limit: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);
            let conn = symgraph::db::open(Path::new(db))?;
            let sh = symgraph::query::Shortener::default(); // CLI 는 절대경로 유지 (디버깅 용도)
            match cmd {
                "find" => symgraph::query::find(&conn, &sh, q, limit),
                "refs" => symgraph::query::refs(&conn, &sh, q, limit),
                "callers" => symgraph::query::callers(&conn, &sh, q, limit),
                "callees" => symgraph::query::callees(&conn, &sh, q, limit),
                "members" => symgraph::query::members(&conn, &sh, q, limit),
                "outline" => symgraph::query::outline(&conn, &sh, q, limit),
                _ => symgraph::query::hierarchy(&conn, &sh, q, limit),
            }
        }
        _ => bail!("{usage}"),
    }
}
