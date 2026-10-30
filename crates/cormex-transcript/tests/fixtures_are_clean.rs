//! The committed fixtures must not contain private content.
//!
//! # Why this is a test and not a one-time check
//!
//! A fixture is a file in a **public repository**, and git history is forever:
//! `.gitignore` cannot take back a leak, and neither can a later commit. The
//! sampler refuses to write a leaking fixture, but that guard runs on the
//! machine that generated it — this one runs on every `cargo test`, including
//! in CI, on whatever is actually committed.
//!
//! # What it cannot check
//!
//! Whether a synthetic string happens to match something real. That is why the
//! PR that adds fixtures says a human looked at the output: the automated check
//! is a floor, not a ceiling.

use std::path::Path;

fn fixture_files() -> Vec<std::path::PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    for agent in ["claude_code", "codex"] {
        let dir = root.join(agent);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "jsonl") {
                out.push(e.path());
            }
        }
    }
    out
}

/// Every line is valid JSON.
///
/// A fixture that does not parse tests nothing: the parser's first step would
/// reject it, and the test would pass by exercising the error path.
#[test]
fn every_fixture_line_is_valid_json() {
    let files = fixture_files();
    assert!(
        !files.is_empty(),
        "no fixtures found -- the path is wrong, or P2-0's output was never committed"
    );
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("{}:{} is not valid JSON: {e}", path.display(), i + 1));
        }
    }
}

/// No home directory other than the placeholder.
#[test]
fn no_fixture_contains_a_real_home_directory() {
    for path in fixture_files() {
        let text = std::fs::read_to_string(&path).expect("read");
        if let Some(idx) = text.find("/home/") {
            let rest = &text[idx + 6..];
            let name: String = rest
                .chars()
                .take_while(|c| *c != '/' && *c != '"' && *c != '\\')
                .collect();
            assert_eq!(
                name,
                "user",
                "{} contains a home directory named {name:?}; the anonymizer's placeholder is \
                 /home/user, so this is either a bug or a path that arrived through a key it \
                 does not know",
                path.display()
            );
        }
    }
}

/// No fixture contains a credential-shaped string.
///
/// The memory crate's own rules, run over the committed bytes: a fixture is
/// public, and a key in it is a key in the git history.
#[test]
fn no_fixture_contains_a_credential() {
    for path in fixture_files() {
        let text = std::fs::read_to_string(&path).expect("read");
        for (i, line) in text.lines().enumerate() {
            if let Some(rule) = cormex_memory::redaction::scan(line) {
                panic!(
                    "{}:{} matches the {} redaction rule -- a fixture is public, and a key in it \
                     is a key in the git history",
                    path.display(),
                    i + 1,
                    rule.name()
                );
            }
        }
    }
}

/// The manifest exists and says where each fixture came from.
#[test]
fn the_manifest_names_every_fixture() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let manifest = std::fs::read_to_string(root.join("MANIFEST.md")).expect("the manifest");
    for path in fixture_files() {
        let name = path.file_name().expect("a name").to_string_lossy();
        assert!(
            manifest.contains(name.as_ref()),
            "the manifest does not mention {name}; a fixture whose origin is not written down \
             cannot be regenerated or audited"
        );
    }
}
