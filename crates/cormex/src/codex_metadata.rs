//! Read Codex's session index and desktop sidebar metadata (titles, project
//! assignments, pins). Every sync reads it for every Codex home; see
//! `sync::metadata`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use cormex_proto::capture::SessionUiMetadata;
use serde_json::Value;

// Codex can persist Windows extended paths in its thread index while the
// sidebar stores normal paths. Compare exact roots after lexical normalization.
fn same_project_root(left: &str, right: &str) -> bool {
    fn normalized(path: &str) -> String {
        let slashes = path.replace('\\', "/");
        let plain = if let Some(rest) = slashes.strip_prefix("//?/UNC/") {
            format!("//{rest}")
        } else {
            slashes.strip_prefix("//?/").unwrap_or(&slashes).to_owned()
        };
        plain.trim_end_matches('/').to_ascii_lowercase()
    }
    normalized(left) == normalized(right)
}

/// Returns metadata keyed by the native session ID. Unknown fields are ignored.
pub fn read(home: &Path) -> Result<HashMap<String, SessionUiMetadata>> {
    let mut dbs = std::fs::read_dir(home)?
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, e.path()))
        })
        .collect::<Vec<_>>();
    dbs.sort_by_key(|x| x.0);
    let Some((_, db)) = dbs.last() else {
        return Ok(HashMap::new());
    };
    let sidebar: Value = match std::fs::read(home.join(".codex-global-state.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid Codex sidebar state")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e.into()),
    };
    let pinned = sidebar.get("pinned-thread-ids").and_then(Value::as_array);
    let projects = sidebar.get("local-projects").and_then(Value::as_object);
    let assignments = sidebar
        .get("thread-project-assignments")
        .and_then(Value::as_object);
    let projectless = sidebar
        .get("projectless-thread-ids")
        .and_then(Value::as_array);
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut stmt = conn.prepare("SELECT id,cwd,name,title,is_pinned,project_id FROM threads")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (id, cwd, name, title, db_pin, db_project) = row?;
        let explicitly_projectless =
            projectless.is_some_and(|xs| xs.iter().any(|x| x.as_str() == Some(&id)));
        let assigned = assignments
            .and_then(|a| a.get(&id))
            .and_then(|a| a.get("projectId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(db_project);
        let exact_root = if assigned.is_none() && !explicitly_projectless {
            projects.and_then(|all| {
                all.iter()
                    .find(|(_, p)| {
                        p.get("rootPaths")
                            .and_then(Value::as_array)
                            .is_some_and(|roots| {
                                roots
                                    .iter()
                                    .any(|r| r.as_str().is_some_and(|r| same_project_root(r, &cwd)))
                            })
                    })
                    .map(|(id, _)| id.clone())
            })
        } else {
            None
        };
        let project_id = assigned.or(exact_root);
        let project_name = project_id
            .as_ref()
            .and_then(|id| projects.and_then(|p| p.get(id)))
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let pinned_position = pinned
            .and_then(|xs| xs.iter().position(|x| x.as_str() == Some(&id)))
            .and_then(|n| i32::try_from(n).ok())
            .or(if db_pin != 0 { Some(i32::MAX) } else { None });
        let title = name
            .filter(|s| !s.trim().is_empty())
            .or_else(|| (!title.trim().is_empty()).then_some(title));
        result.insert(
            id,
            SessionUiMetadata {
                title,
                project_id,
                project_name,
                pinned_position,
            },
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_extended_paths_match_only_the_same_root() {
        assert!(same_project_root(
            r"\\?\C:\Users\a\repo",
            "c:/Users/a/repo/"
        ));
        assert!(same_project_root(
            r"\\?\UNC\server\share\repo",
            r"\\server\share\repo"
        ));
        assert!(!same_project_root(r"\\?\C:\repo\worktree", r"C:\repo"));
    }
    #[test]
    fn reads_exact_titles_assignments_pins_and_explicit_projectless() {
        let home = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads(id TEXT,cwd TEXT,name TEXT,title TEXT,is_pinned INTEGER,project_id TEXT); INSERT INTO threads VALUES('one','D:/repo','Original name','Old fallback',0,NULL),('two','D:/repo',NULL,'Fallback title',0,NULL),('three','D:/repo',NULL,'Third',0,NULL);").unwrap();
        conn.execute(
            "UPDATE threads SET cwd=?1 WHERE id='three'",
            [r"\\?\D:\repo"],
        )
        .unwrap();
        std::fs::write(
            home.path().join(".codex-global-state.json"),
            serde_json::json!({
                "local-projects": {"p": {"name": "Real project", "rootPaths": ["D:/repo"]}},
                "thread-project-assignments": {"one": {"projectId": "p"}},
                "pinned-thread-ids": ["one"], "projectless-thread-ids": ["two"]
            })
            .to_string(),
        )
        .unwrap();
        let records = read(home.path()).unwrap();
        assert_eq!(records["one"].title.as_deref(), Some("Original name"));
        assert_eq!(records["one"].project_name.as_deref(), Some("Real project"));
        assert_eq!(records["one"].pinned_position, Some(0));
        assert_eq!(records["two"].title.as_deref(), Some("Fallback title"));
        assert!(records["two"].project_id.is_none());
        assert_eq!(records["three"].project_id.as_deref(), Some("p"));
    }
}
