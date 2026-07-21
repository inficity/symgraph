//! symgraph — clang index-store 기반 정확 소스 심볼 그래프.
//!
//! 파이프라인: 빌드가 `-index-store-path` 로 남긴 스토어(unit/record)를
//! `merge` 가 SQLite 로 증분 병합하고, `query` 가 그래프를 조회한다.
//! 인덱싱 주체는 clang 자신이므로 이 크레이트는 파서를 갖지 않는다 — 정확도의 근거.
//!
//! 사용처: `symgraph` CLI(빌드 스텝), uemcp MCP 서버(에이전트 쿼리).

pub mod db;
pub mod ffi;
pub mod merge;
pub mod query;

pub use merge::{MergeStats, merge};
