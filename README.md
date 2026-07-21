# symgraph — clang index-store 기반 정확 소스 심볼 그래프

clang 의 index-while-building 부산물을 SQLite 그래프로 증분 병합해, AI 에이전트/도구가
grep 없이 정의·참조·콜러/콜리·상속을 정확하게 조회하게 하는 시스템.
이 문서는 다른 코드베이스에 같은 시스템을 구축할 때의 참조 가이드다.

## 원리

인덱싱 주체는 clang 자신이다. 컴파일 시 `-index-store-path <dir>` 를 주면 clang 이
TU 마다 인덱스 데이터를 스토어에 남긴다. 별도 파서가 없으므로:

- **정확도 100%** — 실제 컴파일된 AST 기준. 매크로 전개, 템플릿 인스턴스, 오버로드
  해소가 전부 반영된 USR(Unified Symbol Resolution) 단위.
- **증분이 공짜** — 빌드가 재컴파일한 TU 만 스토어가 갱신된다. 머저는 차분만 읽는다.
- **데몬 불필요** — 산출물은 디스크의 SQLite. 쿼리 프로세스는 필요할 때만 뜬다.

스토어 구조 (`<store>/v5/`):

- `units/<출력파일명>-<해시>` — TU 하나. main file, 참조하는 record 목록.
  같은 출력 경로면 같은 이름으로 덮어써진다 → **파일 mtime 이 곧 변경 감지 키**.
- `records/<XX>/<파일명>-<해시>` — 소스 파일 하나의 심볼 occurrence 들.
  **content-addressed**: 내용이 같으면 이름이 같다 → 헤더가 안 바뀌면 재파싱 자체가 없다.

## 요구사항

- `-index-store-path` 를 지원하는 clang. Apple clang(Xcode)은 기본 지원.
- 스토어 읽기용 `libIndexStore.dylib` — Xcode 툴체인에 포함
  (`$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib/`).
  C API 시그니처 원본은 swiftlang/indexstore-db 의 `indexstore_functions.h`.
- Rust (이 크레이트: rusqlite bundled + libloading. 링크 타임 의존 없음 — dlopen).

## 파이프라인

```
빌드 (clang -index-store-path <store>)
  → 재컴파일된 TU 만 unit 갱신
빌드 후: symgraph update <store> <db>       ← 증분 병합 (차분만)
  → SQLite 심볼 그래프
쿼리: symgraph find|refs|callers|callees|hierarchy|members|outline <db> <query>
  → 또는 lib 으로 임베드 (MCP 서버 등)
```

## 구축 절차

### 1. 빌드에 플래그 주입

일반 빌드시스템은 `CXXFLAGS += -index-store-path /abs/path/store`.
PCH 를 쓰면 **PCH 생성 커맨드에도 같은 플래그가 걸려야** PCH 안 헤더들이 인덱싱된다
(타깃 전체 컴파일 인자로 걸면 자동으로 해결).

Unreal Engine (배포 엔진) 의 경우 — 프로젝트 `*.Target.cs`:

```csharp
if (Target.Platform == UnrealTargetPlatform.Mac && Target.ProjectFile != null)
{
    string StoreDir = System.IO.Path.Combine(
        Target.ProjectFile.Directory.FullName, "Intermediate", "IndexStore");
    AdditionalCompilerArguments = "-index-store-path " + StoreDir;
    // 배포 엔진의 공유 빌드 환경은 컴파일 인자 변경을 거부한다.
    // -index-store-path 는 코드젠 무영향이라 오버라이드가 안전.
    bOverrideBuildEnvironment = true;
}
```

주의:

- 스토어 경로에 **공백 금지** (UE MacToolChain 이 인자를 공백 split).
- 플래그 추가 직후 첫 빌드는 액션 캐시 무효화로 **풀 리빌드 1회** 발생.
- 인덱스는 해당 빌드 구성의 진실이다. 컴파일되지 않는 분기(`#if` 타 플랫폼)는 안 잡힌다.
- 프리빌트 라이브러리(엔진 등)는 **헤더만** 인덱싱된다 — 자기 코드→라이브러리 방향의
  선언·인라인·호출 엣지는 정확히 잡히지만, 라이브러리 내부 구현(.cpp)은 없다.

### 2. 증분 병합

```
symgraph update <store-dir> <db-path> [--full]
```

빌드 후처리로 체인하면 그래프가 항상 최신이다. 병합 알고리즘:

1. `units/` 디렉터리 fs 스캔 → (이름, mtime) 을 DB 와 diff. mtime 동일 unit 은 열지 않는다.
2. 변경 unit 만 libIndexStore 로 읽어 record 의존 목록을 얻는다.
   record 이름이 DB 에 이미 있으면(content-addressed) 링크만 걸고 재파싱하지 않는다.
3. 사라진 unit(스토어에서 제거되거나 main file 이 삭제된 것)을 지우고,
   어느 unit 도 참조하지 않는 record → occurrence → 고아 symbol 순으로 GC.
4. 전체가 단일 트랜잭션 + prepared statement.

참고 실측 (UE 프로젝트, 게임 모듈 + 플러그인 6개, Apple Silicon):

| 시나리오 | 결과 |
|---|---|
| 첫 전체 병합 | unit 161개 / occurrence 532만 건 / 12.8s |
| 무변경 재실행 | 17ms |
| TU 1개 재컴파일 후 | 109ms |
| 스토어 / DB 용량 | 226MB / 544MB |

### 3. 쿼리 노출

CLI 는 디버깅용이고, 실사용은 lib 임베드를 상정한다 (MCP stdio 서버 등):

```rust
let stats = symgraph::merge(&store_dir, &db_path, /*full=*/false)?;
let conn = symgraph::db::open(&db_path)?;
let sh = symgraph::query::Shortener::new(vec![
    ("/abs/project/root/".into(), "".into()),      // 프로젝트 상대경로로 축약
    ("/abs/engine/root/".into(), "ENGINE/".into()),
]);
symgraph::query::callers(&conn, &sh, "AClassName::Method", 50)?;
```

## 쿼리 시맨틱 (에이전트 UX 설계 포함)

- 쿼리 입력: USR(`c:@...`) 정확 매치 / `Class::Member` qualified name(USR 세그먼트
  패턴 매치) / simple name(정확 → 서브스트링 순).
- 이름이 모호하면 소수(5건 이하)는 후보별 결과를 묶어 바로 반환(왕복 절약),
  다수면 USR 후보 목록을 안내.
- `callers`/`callees` 는 occurrence 의 `CALLEDBY|CONTAINEDBY` relation 기반.
  **가상 호출은 선언 타입의 심볼로 잡힌다** — override 의 콜러가 0이면 base 메서드로
  질의하고, `hierarchy` 의 overridden-by 로 확장하는 게 정확한 사용법.
- `members` 는 `CHILDOF` relation, `outline` 은 파일의 decl/def occurrence.
  둘 다 UE UHT 보일러플레이트(`StaticClass`, `exec*`, `Z_Construct*` 등) 필터 기본 적용.
- 결과가 limit 에 잘리면 `(N total, showing M)` 을 명시해 오판을 막는다.

## 스키마 요약

```
units(name UNIQUE, main_file_id, module_name, mtime_ns)
unit_records(unit_id, record_id)          -- unit ↔ record 링크 (GC 기준)
records(name UNIQUE, file_id)             -- content-addressed
files(path UNIQUE)
symbols(usr UNIQUE, name, kind, lang)
occurrences(record_id, symbol_id, roles, line, col)
relations(occ_id, roles, symbol_id)       -- CALLEDBY/BASEOF/OVERRIDEOF/CHILDOF …
```

스키마 버전이 바뀌면 마이그레이션 없이 전체 재생성한다 — 그래프는 스토어에서
언제든 재구축 가능한 파생 데이터다.

## 알려진 제약

- macOS 검증 완료. Windows 는 clang(-cl) 의 index-store 지원과 libIndexStore 확보가
  선결 과제 (LLVM 배포판 동봉 여부 확인 필요).
- MSVC 는 index store 를 생성하지 못한다 — clang 계열 툴체인 전제.
- 스토어는 스스로 줄어들지 않는다. 장기 사용 시 스토어 삭제 후 리빌드 또는
  `--full` 재병합으로 리셋한다.
