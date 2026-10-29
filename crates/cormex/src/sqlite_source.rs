//! Reading a SQLite source's rows as logical files (P4-0).
//!
//! # Why rows are their own logical files
//!
//! A host that keeps its history in SQLite (Cursor's `state.vscdb`, OpenCode's
//! `opencode.db`) has no file to append to. Each row is a snapshot: it is
//! rewritten in place, and there is nothing to tail. So each row becomes one
//! logical file with `role = row` and `framing = snapshot`, and its logical path
//! is `sqlite:<db relative path>#<key>` — stable across runs, which is what
//! makes the deduplication work.
//!
//! # Read-only, and why that is not just politeness
//!
//! The host has the database open. Opening it read-write would let a bug here
//! corrupt somebody's editor state, and the whole point of this client is that
//! it never modifies what it reads. `mode=ro` is enforced by SQLite itself, so
//! it holds even if a query in the catalog turns out to be an `UPDATE`.
//!
//! `busy_timeout` matters for the same reason: a host writing in
//! rollback-journal mode locks the whole database for the duration of a write,
//! and a zero timeout would fail the run every time that happened to coincide.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cormex_proto::catalog::SqliteSpec;

/// One row, as a logical file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFile {
    /// `sqlite:<db rel path>#<key>`.
    pub logical_path: String,
    /// The bytes to store: the text column when the spec names one, otherwise
    /// the whole row as a JSON object.
    pub bytes: Vec<u8>,
}

/// Read every row `spec` selects out of `db_path`.
///
/// `db_rel` is the database's path relative to the agent's home, and is what
/// goes into the logical path — the absolute path would make the same row look
/// like two different files on two machines.
///
/// # Errors
/// The database cannot be opened read-only, the query fails, or the spec names
/// a column that is not in the result.
pub fn read_rows(db_path: &Path, db_rel: &str, spec: &SqliteSpec) -> Result<Vec<RowFile>> {
    // A URI, because `mode=ro` is a URI parameter. `immutable=1` is
    // deliberately **not** set: it tells SQLite the file cannot change, which
    // is false for a database a running editor is writing to
    let uri = format!("file:{}?mode=ro", db_path.display());
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("cannot open {} read-only", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(spec.busy_timeout_ms))
        .context("cannot set busy_timeout")?;

    let mut stmt = conn
        .prepare(&spec.query)
        .with_context(|| format!("the catalog's query does not prepare: {}", spec.query))?;
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();

    let key_idx = columns
        .iter()
        .position(|c| *c == spec.key_column)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the catalog says the key column is {:?}, but the query returns {columns:?}",
                spec.key_column
            )
        })?;
    let text_idx = match &spec.text_column {
        Some(name) => Some(columns.iter().position(|c| c == name).ok_or_else(|| {
            anyhow::anyhow!(
                "the catalog says the text column is {name:?}, but the query returns {columns:?}"
            )
        })?),
        None => None,
    };

    let mut rows = stmt.query([]).context("the query failed")?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().context("reading a row failed")? {
        let key = value_as_string(row, key_idx)?;
        let bytes = match text_idx {
            Some(i) => value_as_string(row, i)?.into_bytes(),
            // **The whole row**, as a JSON object. A schema we do not model is
            // still data worth keeping: the server's parser decides later what
            // is in it, and the raw bytes are what make that possible
            None => {
                let mut obj = serde_json::Map::new();
                for (i, name) in columns.iter().enumerate() {
                    obj.insert(name.clone(), value_as_json(row, i)?);
                }
                serde_json::to_vec(&serde_json::Value::Object(obj))
                    .context("serializing a row failed")?
            }
        };
        out.push(RowFile {
            logical_path: format!("sqlite:{db_rel}#{key}"),
            bytes,
        });
    }
    Ok(out)
}

/// A column as a string, for the key and text columns.
///
/// BLOB columns are the one case that cannot become a string honestly: a
/// binary key would be mangled by any encoding we chose, so it is hex-encoded
/// with a marker — stable across runs, which is all the key needs.
fn value_as_string(row: &rusqlite::Row<'_>, idx: usize) -> Result<String> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx).context("reading a column failed")? {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("blob:{}", hex_encode(b)),
    })
}

/// A column as JSON, preserving its type where JSON has one.
fn value_as_json(row: &rusqlite::Row<'_>, idx: usize) -> Result<serde_json::Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx).context("reading a column failed")? {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::from(i),
        ValueRef::Real(f) => serde_json::Value::from(f),
        ValueRef::Text(t) => serde_json::Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => serde_json::Value::from(format!("blob:{}", hex_encode(b))),
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The database's path relative to the agent's home, with forward slashes.
///
/// The absolute path must not go into the logical path: the same row on two
/// machines would then look like two different files, and the cross-device
/// merge would do nothing.
#[must_use]
pub fn db_rel_path(home: &Path, db: &Path) -> String {
    db.strip_prefix(home)
        .map(|rel| {
            rel.components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_else(|_| db.display().to_string())
}

/// The physical path of a SQLite source, given the agent's home and the spec's
/// `glob`.
///
/// A SQLite source's `glob` names one database, not a pattern. It is still a
/// `glob` field so that the catalog has one shape for both kinds of source.
#[must_use]
pub fn db_path(home: &Path, glob: &str) -> PathBuf {
    home.join(glob)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small database with two rows.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.execute_batch(
            "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO kv VALUES ('a', '{\"text\": \"hello\"}'), ('b', '{\"text\": \"world\"}');",
        )
        .expect("seed");
        (dir, path)
    }

    fn spec(text_column: Option<&str>) -> SqliteSpec {
        SqliteSpec {
            query: "SELECT key, value FROM kv ORDER BY key".into(),
            key_column: "key".into(),
            text_column: text_column.map(str::to_owned),
            busy_timeout_ms: 1000,
        }
    }

    /// Rows come out with a stable logical path and the text column's bytes.
    #[test]
    fn rows_are_read_with_a_stable_logical_path() {
        let (dir, path) = fixture();
        let rows = read_rows(&path, "state.vscdb", &spec(Some("value"))).expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].logical_path, "sqlite:state.vscdb#a");
        assert_eq!(rows[0].bytes, br#"{"text": "hello"}"#);
        assert_eq!(rows[1].logical_path, "sqlite:state.vscdb#b");
        drop(dir);
    }

    /// With no text column, the whole row is the content.
    ///
    /// A schema we do not model is still data worth keeping: the server's
    /// parser decides later what is in it.
    #[test]
    fn without_a_text_column_the_whole_row_is_the_content() {
        let (dir, path) = fixture();
        let rows = read_rows(&path, "state.vscdb", &spec(None)).expect("read");
        let v: serde_json::Value = serde_json::from_slice(&rows[0].bytes).expect("JSON");
        assert_eq!(v["key"], "a");
        assert_eq!(v["value"], r#"{"text": "hello"}"#);
        drop(dir);
    }

    /// The database is opened **read-only**, so a query that writes fails.
    ///
    /// That is what protects the host's own state: this client never modifies
    /// what it reads, and `mode=ro` enforces it even if a catalog query turns
    /// out to be an UPDATE.
    #[test]
    fn the_database_is_opened_read_only() {
        let (dir, path) = fixture();
        let mut s = spec(None);
        s.query = "DELETE FROM kv RETURNING key".into();
        let err = read_rows(&path, "state.vscdb", &s).expect_err("a write must fail");
        let text = format!("{err:#}");
        assert!(
            text.contains("readonly")
                || text.contains("read-only")
                || text.contains("attempt to write"),
            "the error should say the database is read-only: {text}"
        );
        // And the row is still there
        let conn = rusqlite::Connection::open(&path).expect("open");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM kv", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 2, "the read-only open modified the database");
        drop(dir);
    }

    /// A key column the query does not return is a catalog bug, reported.
    #[test]
    fn a_missing_key_column_is_reported() {
        let (dir, path) = fixture();
        let mut s = spec(Some("value"));
        s.key_column = "nope".into();
        let err = read_rows(&path, "state.vscdb", &s).expect_err("a bad catalog must fail");
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
        drop(dir);
    }

    /// A text column the query does not return is reported too.
    #[test]
    fn a_missing_text_column_is_reported() {
        let (dir, path) = fixture();
        let mut s = spec(Some("nope"));
        s.text_column = Some("nope".into());
        assert!(read_rows(&path, "state.vscdb", &s).is_err());
        drop(dir);
    }

    /// The relative path goes into the logical path, not the absolute one.
    ///
    /// The same row on two machines must look like the same file.
    #[test]
    fn the_logical_path_uses_the_relative_path() {
        let home = Path::new("/home/u/.cursor");
        let db = Path::new("/home/u/.cursor/state.vscdb");
        assert_eq!(db_rel_path(home, db), "state.vscdb");
        assert_eq!(
            db_rel_path(home, Path::new("/home/u/.cursor/x/y.db")),
            "x/y.db"
        );
    }
}
