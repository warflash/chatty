//! Minimal SQLite binding to Windows' own `winsqlite3.dll` (System32), so
//! nothing is compiled and nothing extra ships. `raw-dylib` needs no import
//! library.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

#[link(name = "winsqlite3", kind = "raw-dylib")]
unsafe extern "system" {
    fn sqlite3_open_v2(filename: *const c_char, db: *mut *mut c_void, flags: c_int, vfs: *const c_char) -> c_int;
    fn sqlite3_close_v2(db: *mut c_void) -> c_int;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut c_void, ms: c_int) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut c_void, i: c_int, v: i64) -> c_int;
    fn sqlite3_bind_double(stmt: *mut c_void, i: c_int, v: f64) -> c_int;
    fn sqlite3_bind_text(stmt: *mut c_void, i: c_int, v: *const c_char, n: c_int, destructor: isize) -> c_int;
    fn sqlite3_bind_null(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_type(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut c_void, i: c_int) -> i64;
    fn sqlite3_column_double(stmt: *mut c_void, i: c_int) -> f64;
    fn sqlite3_column_text(stmt: *mut c_void, i: c_int) -> *const u8;
    fn sqlite3_column_bytes(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_last_insert_rowid(db: *mut c_void) -> i64;
}

const OK: c_int = 0;
const ROW: c_int = 100;
const DONE: c_int = 101;
const OPEN_READWRITE: c_int = 0x2;
const OPEN_CREATE: c_int = 0x4;
const OPEN_FULLMUTEX: c_int = 0x10000;
const TRANSIENT: isize = -1;
const INTEGER: c_int = 1;
const FLOAT: c_int = 2;
const TEXT: c_int = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl Value {
    pub fn int(&self) -> i64 {
        match self {
            Value::Int(v) => *v,
            Value::Real(v) => *v as i64,
            Value::Text(t) => t.parse().unwrap_or(0),
            Value::Null => 0,
        }
    }

    pub fn real(&self) -> f64 {
        match self {
            Value::Int(v) => *v as f64,
            Value::Real(v) => *v,
            Value::Text(t) => t.parse().unwrap_or(0.0),
            Value::Null => 0.0,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Value::Text(t) => t.clone(),
            Value::Int(v) => v.to_string(),
            Value::Real(v) => v.to_string(),
            Value::Null => String::new(),
        }
    }

    pub fn is_null(&self) -> bool {
        *self == Value::Null
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        if v.is_finite() { Value::Real(v) } else { Value::Null }
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

pub struct Db(*mut c_void);

// SQLITE_OPEN_FULLMUTEX: the connection serializes its own calls.
unsafe impl Send for Db {}

pub type Row = Vec<Value>;

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Db, String> {
        let c = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let mut db = std::ptr::null_mut();
        let rc = unsafe { sqlite3_open_v2(c.as_ptr(), &mut db, OPEN_READWRITE | OPEN_CREATE | OPEN_FULLMUTEX, std::ptr::null()) };
        let db = Db(db);
        if rc != OK {
            return Err(db.error());
        }
        unsafe { sqlite3_busy_timeout(db.0, 2000) };
        Ok(db)
    }

    fn error(&self) -> String {
        if self.0.is_null() {
            return "sqlite: out of memory".into();
        }
        unsafe { CStr::from_ptr(sqlite3_errmsg(self.0)) }.to_string_lossy().into_owned()
    }

    /// Runs every statement in `sql` (no parameters, results ignored).
    pub fn exec_batch(&self, sql: &str) -> Result<(), String> {
        let c = CString::new(sql).map_err(|e| e.to_string())?;
        let mut tail: *const c_char = c.as_ptr();
        loop {
            let rest = unsafe { CStr::from_ptr(tail) };
            if rest.to_bytes().iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            let mut stmt = std::ptr::null_mut();
            let mut next: *const c_char = std::ptr::null();
            if unsafe { sqlite3_prepare_v2(self.0, tail, -1, &mut stmt, &mut next) } != OK {
                return Err(self.error());
            }
            if !stmt.is_null() {
                let rc = loop {
                    let rc = unsafe { sqlite3_step(stmt) };
                    if rc != ROW {
                        break rc;
                    }
                };
                unsafe { sqlite3_finalize(stmt) };
                if rc != DONE {
                    return Err(self.error());
                }
            }
            tail = next;
        }
    }

    fn prepare(&self, sql: &str, params: &[Value]) -> Result<*mut c_void, String> {
        let mut stmt = std::ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(self.0, sql.as_ptr() as *const c_char, sql.len() as c_int, &mut stmt, std::ptr::null_mut())
        };
        if rc != OK {
            return Err(format!("{}: {sql}", self.error()));
        }
        for (i, p) in params.iter().enumerate() {
            let i = i as c_int + 1;
            unsafe {
                match p {
                    Value::Null => sqlite3_bind_null(stmt, i),
                    Value::Int(v) => sqlite3_bind_int64(stmt, i, *v),
                    Value::Real(v) => sqlite3_bind_double(stmt, i, *v),
                    Value::Text(t) => {
                        sqlite3_bind_text(stmt, i, t.as_ptr() as *const c_char, t.len() as c_int, TRANSIENT)
                    }
                };
            }
        }
        Ok(stmt)
    }

    /// Executes one statement; returns the last insert rowid.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<i64, String> {
        let stmt = self.prepare(sql, params)?;
        let rc = unsafe { sqlite3_step(stmt) };
        unsafe { sqlite3_finalize(stmt) };
        if rc != DONE && rc != ROW {
            return Err(format!("{}: {sql}", self.error()));
        }
        Ok(unsafe { sqlite3_last_insert_rowid(self.0) })
    }

    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, String> {
        let stmt = self.prepare(sql, params)?;
        let mut rows = Vec::new();
        loop {
            match unsafe { sqlite3_step(stmt) } {
                ROW => {
                    let n = unsafe { sqlite3_column_count(stmt) };
                    let row = (0..n)
                        .map(|i| unsafe {
                            match sqlite3_column_type(stmt, i) {
                                INTEGER => Value::Int(sqlite3_column_int64(stmt, i)),
                                FLOAT => Value::Real(sqlite3_column_double(stmt, i)),
                                TEXT => {
                                    let p = sqlite3_column_text(stmt, i);
                                    let len = sqlite3_column_bytes(stmt, i) as usize;
                                    Value::Text(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned())
                                }
                                _ => Value::Null,
                            }
                        })
                        .collect();
                    rows.push(row);
                }
                DONE => break,
                _ => {
                    let e = self.error();
                    unsafe { sqlite3_finalize(stmt) };
                    return Err(format!("{e}: {sql}"));
                }
            }
        }
        unsafe { sqlite3_finalize(stmt) };
        Ok(rows)
    }

    /// First column of the first row.
    pub fn scalar(&self, sql: &str, params: &[Value]) -> Result<Value, String> {
        Ok(self.query(sql, params)?.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close_v2(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join(format!("chatty-sqlite-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        db.exec_batch("CREATE TABLE t(a INTEGER, b REAL, c TEXT); CREATE INDEX ti ON t(a);").unwrap();
        let id = db.execute("INSERT INTO t VALUES (?, ?, ?)", &[5i64.into(), 1.5.into(), "äh 🎤".into()]).unwrap();
        assert_eq!(id, 1);
        db.execute("INSERT INTO t VALUES (?, ?, ?)", &[Value::Null, Value::Null, Value::Null]).unwrap();
        let rows = db.query("SELECT a, b, c FROM t ORDER BY rowid", &[]).unwrap();
        assert_eq!(rows[0], vec![Value::Int(5), Value::Real(1.5), Value::Text("äh 🎤".into())]);
        assert!(rows[1][0].is_null());
        assert_eq!(db.scalar("SELECT count(*) FROM t", &[]).unwrap().int(), 2);
        assert!(db.query("SELECT nope FROM t", &[]).is_err());
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
