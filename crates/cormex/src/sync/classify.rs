//! Derive the stable project key sent with a discovered file.

use std::collections::HashMap;
use std::path::Path;

use cormex_proto::capture::FileClassification;

use crate::catalog::DiscoveredFile;
use crate::config::Config;
use crate::git_meta;
use crate::sync::reader::LogicalReader;

/// A per-run cache keyed by normalized cwd. Git metadata is expensive enough
/// that discovering two hundred sessions in one repository must not spawn four
/// hundred git commands.
pub type ProjectCache = HashMap<String, Option<String>>;

/// Enough of the first JSONL records to reach the host's cwd metadata without
/// reading a whole transcript on every scheduled sync.
const CWD_PROBE_BYTES: usize = 32 * 1024;

/// Build the metadata for one file.
///
/// A hook-supplied cwd may be used for git lookup. A path-decoded fallback is
/// only classified with a local key: the Claude directory encoding is lossy,
/// and guessing a different repository from it would be worse than a local
/// grouping.
#[must_use]
pub fn for_file(
    file: &DiscoveredFile,
    cfg: &Config,
    cache: &mut ProjectCache,
) -> Option<FileClassification> {
    if !cfg.git_metadata {
        return None;
    }
    let (cwd, trusted) = match file.cwd.as_deref() {
        Some(cwd) if !file.cwd_guessed => (normalize_cwd(cwd)?, true),
        supplied => transcript_cwd(file)
            .map(|cwd| (cwd, true))
            .or_else(|| supplied.and_then(|cwd| normalize_cwd(cwd).map(|cwd| (cwd, false))))?,
    };
    let git_key = trusted
        .then(|| {
            cache
                .entry(cwd.clone())
                .or_insert_with(|| git_meta::project_key(Path::new(&cwd)))
                .clone()
        })
        .flatten();
    let project_key = git_key.or_else(|| local_project_key(&cwd));
    Some(FileClassification {
        cwd: Some(cwd),
        project_key,
    })
}

/// Read the host's own cwd metadata from the first few transcript records.
///
/// Codex puts it in `session_meta.payload.cwd`; Claude Code puts it at the top
/// level. This is the only way to repair files captured before the hook's cwd
/// was persisted: a Codex rollout path contains no project name at all, and
/// Claude's encoded project directory is lossy.
fn transcript_cwd(file: &DiscoveredFile) -> Option<String> {
    if !matches!(
        file.role,
        cormex_proto::capture::FileRole::Transcript | cormex_proto::capture::FileRole::Subagent
    ) {
        return None;
    }
    let bytes = match &file.bytes {
        Some(bytes) => bytes
            .get(..bytes.len().min(CWD_PROBE_BYTES))
            .unwrap_or(bytes)
            .to_vec(),
        None => LogicalReader::open(&file.physical_path, file.compressed)
            .ok()?
            .read_range(0, CWD_PROBE_BYTES)
            .ok()?,
    };
    for line in bytes.split(|byte| *byte == b'\n').take(32) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let cwd = value
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                value
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(serde_json::Value::as_str)
            });
        if let Some(cwd) = cwd.and_then(normalize_cwd) {
            return Some(cwd);
        }
    }
    None
}

/// A stable, machine-local grouping key for a cwd outside a git repository.
#[must_use]
pub fn local_project_key(cwd: &str) -> Option<String> {
    let cwd = normalize_cwd(cwd)?;
    let key = format!("local:{cwd}");
    (key.chars().count() <= 512).then_some(key)
}

/// Normalize slash direction, Windows extended prefixes, duplicate separators,
/// trailing separators, and the drive-letter case.
#[must_use]
pub fn normalize_cwd(raw: &str) -> Option<String> {
    let mut path = raw.trim().replace('\\', "/");
    if path.is_empty() {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("//?/unc/") {
        path = format!("//{}", &path[8..]);
    } else if lower.starts_with("//?/") {
        path = path[4..].to_owned();
    } else if lower.starts_with("/?/") {
        path = path[3..].to_owned();
    }

    // A UNC share's leading `//` is semantic: collapsing it would turn
    // `//server/share` into the local path `/server/share`.
    let (prefix, rest) = if let Some(rest) = path.strip_prefix("//") {
        ("//", rest)
    } else if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else {
        ("", path.as_str())
    };
    let body = rest
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if prefix == "//" && body.is_empty() {
        return None;
    }
    path = format!("{prefix}{body}");
    if path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
    {
        let mut bytes = path.into_bytes();
        bytes[0] = bytes[0].to_ascii_lowercase();
        path = String::from_utf8(bytes).ok()?;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_proto::capture::{FileRole, Framing};

    fn transcript(path: &Path) -> DiscoveredFile {
        DiscoveredFile {
            agent: "codex".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            logical_path: "sessions/s.jsonl".into(),
            physical_path: path.to_path_buf(),
            compressed: false,
            native_session_id: "s".into(),
            parent_native_session_id: None,
            cwd: None,
            cwd_guessed: false,
            bytes: None,
        }
    }

    #[test]
    fn local_keys_are_stable_across_path_spellings() {
        assert_eq!(
            local_project_key(r"\\?\D:\Codes\Project\").as_deref(),
            Some("local:d:/Codes/Project")
        );
        assert_eq!(
            local_project_key("d:/Codes//Project").as_deref(),
            Some("local:d:/Codes/Project")
        );
        assert_eq!(
            local_project_key("/home/u/project/").as_deref(),
            Some("local:/home/u/project")
        );
        assert_eq!(
            local_project_key(r"\\server\share\project").as_deref(),
            Some("local://server/share/project"),
            "collapsing the leading // turns a UNC share into a local path"
        );
        assert_eq!(
            local_project_key(r"\\?\UNC\server\share\project").as_deref(),
            Some("local://server/share/project")
        );
    }

    #[test]
    fn an_empty_cwd_has_no_key() {
        assert_eq!(local_project_key(""), None);
        assert_eq!(normalize_cwd("   "), None);
    }

    #[test]
    fn transcript_metadata_supplies_a_cwd_when_the_path_cannot() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": "s",
                        "cwd": dir.path().to_string_lossy()
                    }
                }),
                serde_json::json!({"type": "response_item"})
            ),
        )
        .expect("write");
        let file = transcript(&path);
        let classification = for_file(&file, &Config::default(), &mut ProjectCache::new())
            .expect("the transcript should be classified");
        assert_eq!(
            classification.project_key.as_deref(),
            local_project_key(&dir.path().to_string_lossy()).as_deref()
        );
    }

    #[test]
    fn transcript_metadata_uses_the_git_project_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("git should be installed for the capture tests");
        assert!(init.success());
        let remote = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/example/repository.git",
            ])
            .current_dir(dir.path())
            .status()
            .expect("git remote should run");
        assert!(remote.success());
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": "s",
                        "cwd": dir.path().to_string_lossy()
                    }
                })
            ),
        )
        .expect("write");

        let classification = for_file(
            &transcript(&path),
            &Config::default(),
            &mut ProjectCache::new(),
        )
        .expect("the transcript should be classified");
        assert_eq!(
            classification.project_key.as_deref(),
            Some("github.com/example/repository"),
            "a real repository must use the same canonical key as every other capture source"
        );
    }

    #[test]
    fn a_guessed_path_never_runs_git_against_the_wrong_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"type\":\"user_message\"}\n").expect("write");
        let mut file = transcript(&path);
        file.cwd = Some("/home/u/not-the-project".into());
        file.cwd_guessed = true;
        let classification = for_file(&file, &Config::default(), &mut ProjectCache::new())
            .expect("guessed cwd still gets a local key");
        assert_eq!(
            classification.project_key.as_deref(),
            Some("local:/home/u/not-the-project")
        );
    }

    #[test]
    fn disabled_git_metadata_produces_no_classification() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"cwd\":\"/p\"}\n").expect("write");
        let cfg = Config {
            git_metadata: false,
            ..Config::default()
        };
        assert_eq!(
            for_file(&transcript(&path), &cfg, &mut ProjectCache::new()),
            None
        );
    }
}
