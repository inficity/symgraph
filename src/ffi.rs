//! libIndexStore(clang index-while-building 스토어 읽기 공식 C API) 동적 바인딩.
//!
//! 링크 타임 의존 없이 dlopen(libloading)으로 붙는다 — 툴체인이 없는 환경에서도
//! 바이너리 자체는 동작하고, 스토어를 읽는 시점에만 라이브러리를 요구한다.
//! 시그니처 원본: swiftlang/indexstore-db `indexstore_functions.h` (INDEXSTORE_VERSION 0.14).

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use libloading::Library;

/// 불투명 핸들(indexstore_t, reader, symbol, occurrence, relation, dependency, error 공용).
pub type Handle = *mut c_void;

/// `indexstore_string_ref_t` — (ptr, len) 문자열 뷰. 콜백 스코프 안에서만 유효.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StringRef {
    pub data: *const c_char,
    pub length: usize,
}

impl StringRef {
    pub fn to_string(self) -> String {
        if self.data.is_null() || self.length == 0 {
            return String::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.data as *const u8, self.length) };
        String::from_utf8_lossy(bytes).into_owned()
    }
}

// ── symbol role 비트 (occurrence roles / relation roles 공용) ──────────────
pub const ROLE_DECLARATION: u64 = 1 << 0;
pub const ROLE_DEFINITION: u64 = 1 << 1;
pub const ROLE_REFERENCE: u64 = 1 << 2;
pub const ROLE_READ: u64 = 1 << 3;
pub const ROLE_WRITE: u64 = 1 << 4;
pub const ROLE_CALL: u64 = 1 << 5;
pub const ROLE_DYNAMIC: u64 = 1 << 6;
pub const ROLE_ADDRESSOF: u64 = 1 << 7;
pub const ROLE_IMPLICIT: u64 = 1 << 8;
pub const ROLE_REL_CHILDOF: u64 = 1 << 9;
pub const ROLE_REL_BASEOF: u64 = 1 << 10;
pub const ROLE_REL_OVERRIDEOF: u64 = 1 << 11;
pub const ROLE_REL_RECEIVEDBY: u64 = 1 << 12;
pub const ROLE_REL_CALLEDBY: u64 = 1 << 13;
pub const ROLE_REL_CONTAINEDBY: u64 = 1 << 16;
pub const ROLE_REL_SPECIALIZATIONOF: u64 = 1 << 18;

/// `indexstore_unit_dependency_kind_t`
pub const DEP_KIND_UNIT: u32 = 1;
pub const DEP_KIND_RECORD: u32 = 2;
pub const DEP_KIND_FILE: u32 = 3;

/// 콜백 트램펄린 시그니처: (context, item) -> continue?
type ApplierF = unsafe extern "C" fn(*mut c_void, Handle) -> bool;

/// dlopen 된 libIndexStore 의 함수 테이블. `_lib` 가 살아 있는 동안만 유효하다.
pub struct IndexStoreLib {
    _lib: Library,
    pub error_get_description: unsafe extern "C" fn(Handle) -> *const c_char,
    pub error_dispose: unsafe extern "C" fn(Handle),
    pub store_create: unsafe extern "C" fn(*const c_char, *mut Handle) -> Handle,
    pub store_dispose: unsafe extern "C" fn(Handle),
    pub unit_reader_create: unsafe extern "C" fn(Handle, *const c_char, *mut Handle) -> Handle,
    pub unit_reader_dispose: unsafe extern "C" fn(Handle),
    pub unit_reader_has_main_file: unsafe extern "C" fn(Handle) -> bool,
    pub unit_reader_get_main_file: unsafe extern "C" fn(Handle) -> StringRef,
    pub unit_reader_get_module_name: unsafe extern "C" fn(Handle) -> StringRef,
    pub unit_reader_dependencies_apply_f: unsafe extern "C" fn(Handle, *mut c_void, ApplierF) -> bool,
    pub unit_dependency_get_kind: unsafe extern "C" fn(Handle) -> u32,
    pub unit_dependency_is_system: unsafe extern "C" fn(Handle) -> bool,
    pub unit_dependency_get_name: unsafe extern "C" fn(Handle) -> StringRef,
    pub unit_dependency_get_filepath: unsafe extern "C" fn(Handle) -> StringRef,
    pub record_reader_create: unsafe extern "C" fn(Handle, *const c_char, *mut Handle) -> Handle,
    pub record_reader_dispose: unsafe extern "C" fn(Handle),
    pub record_reader_occurrences_apply_f: unsafe extern "C" fn(Handle, *mut c_void, ApplierF) -> bool,
    pub occurrence_get_symbol: unsafe extern "C" fn(Handle) -> Handle,
    pub occurrence_get_roles: unsafe extern "C" fn(Handle) -> u64,
    pub occurrence_get_line_col: unsafe extern "C" fn(Handle, *mut u32, *mut u32),
    pub occurrence_relations_apply_f: unsafe extern "C" fn(Handle, *mut c_void, ApplierF) -> bool,
    pub symbol_relation_get_roles: unsafe extern "C" fn(Handle) -> u64,
    pub symbol_relation_get_symbol: unsafe extern "C" fn(Handle) -> Handle,
    pub symbol_get_kind: unsafe extern "C" fn(Handle) -> u32,
    pub symbol_get_language: unsafe extern "C" fn(Handle) -> u32,
    pub symbol_get_name: unsafe extern "C" fn(Handle) -> StringRef,
    pub symbol_get_usr: unsafe extern "C" fn(Handle) -> StringRef,
}

macro_rules! load_sym {
    ($lib:expr, $name:literal) => {{
        let sym = $lib
            .get(concat!($name, "\0").as_bytes())
            .with_context(|| format!("libIndexStore 심볼 없음: {}", $name))?;
        *sym
    }};
}

impl IndexStoreLib {
    /// libIndexStore 를 dlopen 하고 필요한 심볼을 모두 바인딩한다.
    pub fn load() -> Result<Self> {
        let path = find_dylib()?;
        let lib = unsafe { Library::new(&path) }
            .with_context(|| format!("libIndexStore 로드 실패: {}", path.display()))?;
        unsafe {
            Ok(Self {
                error_get_description: load_sym!(lib, "indexstore_error_get_description"),
                error_dispose: load_sym!(lib, "indexstore_error_dispose"),
                store_create: load_sym!(lib, "indexstore_store_create"),
                store_dispose: load_sym!(lib, "indexstore_store_dispose"),
                unit_reader_create: load_sym!(lib, "indexstore_unit_reader_create"),
                unit_reader_dispose: load_sym!(lib, "indexstore_unit_reader_dispose"),
                unit_reader_has_main_file: load_sym!(lib, "indexstore_unit_reader_has_main_file"),
                unit_reader_get_main_file: load_sym!(lib, "indexstore_unit_reader_get_main_file"),
                unit_reader_get_module_name: load_sym!(lib, "indexstore_unit_reader_get_module_name"),
                unit_reader_dependencies_apply_f: load_sym!(lib, "indexstore_unit_reader_dependencies_apply_f"),
                unit_dependency_get_kind: load_sym!(lib, "indexstore_unit_dependency_get_kind"),
                unit_dependency_is_system: load_sym!(lib, "indexstore_unit_dependency_is_system"),
                unit_dependency_get_name: load_sym!(lib, "indexstore_unit_dependency_get_name"),
                unit_dependency_get_filepath: load_sym!(lib, "indexstore_unit_dependency_get_filepath"),
                record_reader_create: load_sym!(lib, "indexstore_record_reader_create"),
                record_reader_dispose: load_sym!(lib, "indexstore_record_reader_dispose"),
                record_reader_occurrences_apply_f: load_sym!(lib, "indexstore_record_reader_occurrences_apply_f"),
                occurrence_get_symbol: load_sym!(lib, "indexstore_occurrence_get_symbol"),
                occurrence_get_roles: load_sym!(lib, "indexstore_occurrence_get_roles"),
                occurrence_get_line_col: load_sym!(lib, "indexstore_occurrence_get_line_col"),
                occurrence_relations_apply_f: load_sym!(lib, "indexstore_occurrence_relations_apply_f"),
                symbol_relation_get_roles: load_sym!(lib, "indexstore_symbol_relation_get_roles"),
                symbol_relation_get_symbol: load_sym!(lib, "indexstore_symbol_relation_get_symbol"),
                symbol_get_kind: load_sym!(lib, "indexstore_symbol_get_kind"),
                symbol_get_language: load_sym!(lib, "indexstore_symbol_get_language"),
                symbol_get_name: load_sym!(lib, "indexstore_symbol_get_name"),
                symbol_get_usr: load_sym!(lib, "indexstore_symbol_get_usr"),
                _lib: lib,
            })
        }
    }

    fn take_error(&self, err: Handle) -> String {
        if err.is_null() {
            return "unknown indexstore error".into();
        }
        let msg = unsafe {
            let c = (self.error_get_description)(err);
            if c.is_null() {
                "unknown indexstore error".into()
            } else {
                CStr::from_ptr(c).to_string_lossy().into_owned()
            }
        };
        unsafe { (self.error_dispose)(err) };
        msg
    }

    /// 스토어 루트(-index-store-path 로 준 디렉터리)를 연다.
    pub fn open_store(&self, store_dir: &Path) -> Result<Store<'_>> {
        let c_path = CString::new(store_dir.to_string_lossy().as_bytes())?;
        let mut err: Handle = std::ptr::null_mut();
        let h = unsafe { (self.store_create)(c_path.as_ptr(), &mut err) };
        if h.is_null() {
            bail!("indexstore open 실패({}): {}", store_dir.display(), self.take_error(err));
        }
        Ok(Store { lib: self, h })
    }
}

/// 열린 스토어. Drop 에서 dispose 한다.
pub struct Store<'a> {
    lib: &'a IndexStoreLib,
    h: Handle,
}

impl Drop for Store<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.store_dispose)(self.h) };
    }
}

impl Store<'_> {
    pub fn unit_reader(&self, unit_name: &str) -> Result<UnitReader<'_>> {
        let c_name = CString::new(unit_name)?;
        let mut err: Handle = std::ptr::null_mut();
        let h = unsafe { (self.lib.unit_reader_create)(self.h, c_name.as_ptr(), &mut err) };
        if h.is_null() {
            bail!("unit reader 실패({unit_name}): {}", self.lib.take_error(err));
        }
        Ok(UnitReader { lib: self.lib, h })
    }

    pub fn record_reader(&self, record_name: &str) -> Result<RecordReader<'_>> {
        let c_name = CString::new(record_name)?;
        let mut err: Handle = std::ptr::null_mut();
        let h = unsafe { (self.lib.record_reader_create)(self.h, c_name.as_ptr(), &mut err) };
        if h.is_null() {
            bail!("record reader 실패({record_name}): {}", self.lib.take_error(err));
        }
        Ok(RecordReader { lib: self.lib, h })
    }
}

/// FnMut 클로저를 C 콜백으로 넘기는 트램펄린.
unsafe extern "C" fn trampoline<F: FnMut(Handle) -> bool>(ctx: *mut c_void, item: Handle) -> bool {
    let f = unsafe { &mut *(ctx as *mut F) };
    f(item)
}

fn apply_f<F: FnMut(Handle) -> bool>(
    applier: unsafe extern "C" fn(Handle, *mut c_void, ApplierF) -> bool,
    target: Handle,
    mut f: F,
) {
    unsafe { applier(target, &mut f as *mut F as *mut c_void, trampoline::<F>) };
}

pub struct UnitReader<'a> {
    lib: &'a IndexStoreLib,
    h: Handle,
}

impl Drop for UnitReader<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.unit_reader_dispose)(self.h) };
    }
}

/// unit 이 참조하는 record 의존성 한 건.
pub struct RecordDep {
    pub name: String,
    pub file_path: String,
    pub is_system: bool,
}

impl UnitReader<'_> {
    pub fn main_file(&self) -> Option<String> {
        if unsafe { (self.lib.unit_reader_has_main_file)(self.h) } {
            Some(unsafe { (self.lib.unit_reader_get_main_file)(self.h) }.to_string())
        } else {
            None
        }
    }

    pub fn module_name(&self) -> String {
        unsafe { (self.lib.unit_reader_get_module_name)(self.h) }.to_string()
    }

    /// record 의존성만 모아 반환한다 (unit/file 의존성은 그래프에 불필요).
    pub fn record_deps(&self) -> Vec<RecordDep> {
        let lib = self.lib;
        let mut out = Vec::new();
        apply_f(lib.unit_reader_dependencies_apply_f, self.h, |dep| {
            unsafe {
                if (lib.unit_dependency_get_kind)(dep) == DEP_KIND_RECORD {
                    out.push(RecordDep {
                        name: (lib.unit_dependency_get_name)(dep).to_string(),
                        file_path: (lib.unit_dependency_get_filepath)(dep).to_string(),
                        is_system: (lib.unit_dependency_is_system)(dep),
                    });
                }
            }
            true
        });
        out
    }
}

pub struct RecordReader<'a> {
    lib: &'a IndexStoreLib,
    h: Handle,
}

impl Drop for RecordReader<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.record_reader_dispose)(self.h) };
    }
}

/// record 파일 하나에서 읽은 심볼 정보 (occurrence/relation 에서 공용).
pub struct SymInfo {
    pub usr: String,
    pub name: String,
    pub kind: u32,
    pub lang: u32,
}

/// occurrence 한 건 — 심볼 + 위치 + roles + relations.
pub struct Occurrence {
    pub symbol: SymInfo,
    pub roles: u64,
    pub line: u32,
    pub col: u32,
    pub relations: Vec<(u64, SymInfo)>,
}

impl RecordReader<'_> {
    fn read_sym(lib: &IndexStoreLib, sym: Handle) -> SymInfo {
        unsafe {
            SymInfo {
                usr: (lib.symbol_get_usr)(sym).to_string(),
                name: (lib.symbol_get_name)(sym).to_string(),
                kind: (lib.symbol_get_kind)(sym),
                lang: (lib.symbol_get_language)(sym),
            }
        }
    }

    /// record 의 모든 occurrence 를 순회해 콜백에 넘긴다.
    pub fn for_each_occurrence<F: FnMut(Occurrence)>(&self, mut f: F) {
        let lib = self.lib;
        apply_f(lib.record_reader_occurrences_apply_f, self.h, |occ| {
            let (mut line, mut col) = (0u32, 0u32);
            let (symbol, roles) = unsafe {
                (lib.occurrence_get_line_col)(occ, &mut line, &mut col);
                (
                    Self::read_sym(lib, (lib.occurrence_get_symbol)(occ)),
                    (lib.occurrence_get_roles)(occ),
                )
            };
            let mut relations = Vec::new();
            apply_f(lib.occurrence_relations_apply_f, occ, |rel| {
                unsafe {
                    relations.push((
                        (lib.symbol_relation_get_roles)(rel),
                        Self::read_sym(lib, (lib.symbol_relation_get_symbol)(rel)),
                    ));
                }
                true
            });
            f(Occurrence { symbol, roles, line, col, relations });
            true
        });
    }
}

/// libIndexStore 위치를 찾는다: 환경변수 → xcode-select 툴체인 → CommandLineTools.
fn find_dylib() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SYMGRAPH_LIBINDEXSTORE") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
        bail!("SYMGRAPH_LIBINDEXSTORE 가 가리키는 파일이 없음: {}", pb.display());
    }
    let mut candidates = Vec::new();
    if let Ok(out) = Command::new("xcode-select").arg("-p").output() {
        if out.status.success() {
            let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
            candidates.push(PathBuf::from(format!(
                "{dev}/Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib"
            )));
            candidates.push(PathBuf::from(format!("{dev}/usr/lib/libIndexStore.dylib")));
        }
    }
    candidates.push(PathBuf::from(
        "/Library/Developer/CommandLineTools/usr/lib/libIndexStore.dylib",
    ));
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    bail!(
        "libIndexStore.dylib 를 찾지 못함 (SYMGRAPH_LIBINDEXSTORE 로 지정 가능). 시도한 경로: {}",
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )
}
