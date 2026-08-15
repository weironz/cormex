//! Applied migrations are immutable **down to the byte**, comments included.
//!
//! # Why a test rather than a rule in a document
//!
//! There was a rule in a document. It said "do not edit an applied migration;
//! add a new file for a schema change", and it was followed as far as anyone
//! could tell: when this repository was translated to English, a mechanical
//! check confirmed the **SQL statements** in every migration were token-for-token
//! identical afterwards. Only the comments had changed.
//!
//! sqlx does not hash the statements. It hashes **the file**. So the next
//! deployment stopped halfway through with
//!
//! ```text
//! migration 20260807000001 was previously applied but has been modified
//! ```
//!
//! -- on the production node, after the new compose was already in place. The
//! rule was right and the verification was aimed one layer too high.
//!
//! # What this test costs you
//!
//! Adding a migration makes it fail until you paste the new file's hash in
//! below. That is the point: recording the hash is a deliberate act, and it is
//! the moment to ask whether you are adding a file or editing one.
//!
//! Reformatting an applied migration -- even fixing a typo in a comment -- is
//! not a thing you may do. Write the correction into the next migration.

use std::collections::BTreeSet;

/// Every applied migration and the sha256 of its bytes.
const PINNED: &[(&str, &str)] = &[
    (
        "migrations/20260807000001_init.sql",
        "d225b441f5b4bfaa9a3e0e2a1f67466a0b8a4287c99eb38acc0d9296545479f8",
    ),
    (
        "migrations/20260807000002_session_events.sql",
        "4150d0db0df5e332d539f37a7c084d407b01957d204c5892d2b0df16b927c473",
    ),
    (
        "migrations/20260807000003_session_page_cursor.sql",
        "64e69f97f344cadd2f2851fd0e77308c10520298b96f637d027d85e7979f032a",
    ),
    (
        "migrations/20260807000004_attachment_filename.sql",
        "e65d5d9200b589ab353086486fd7403915f763d0ca6ff2a65333a5540a31ce12",
    ),
    (
        "migrations/20260807000005_episode_replay.sql",
        "cbbb7fdb6a96c54d19785aa77dc7d1ebcd580bf58d86a8afd0260ae00b5b00e0",
    ),
    (
        "migrations/20260807000006_fact_provenance.sql",
        "3860596583cfdfd299271340d9cabf5ac13adbd5a21f9ad12591824ce55c4fe7",
    ),
    (
        "migrations/20260808000001_embedding_backfill.sql",
        "e289414209527f0e790b023f5fadbb7810260258d14b19a61b631cfcb0ea6d32",
    ),
    (
        "migrations/20260810000001_active_facts_uses_the_index.sql",
        "f21e61693d02f05045e91343b16d1707c35e5f98aa5b97e9a1774abea0a28d0d",
    ),
    (
        "migrations/20260812000001_byo_llm_key.sql",
        "f451eea97c3b754143ec05199975493fb940d6f9ca82018c39f74cef3b9465a7",
    ),
    (
        "migrations/20260812000002_byo_key_base_url.sql",
        "2ca2cb721a65e46899015d42d9797bcb96b260851faebf59771e4c712c520992",
    ),
    (
        "migrations/20260812000003_projects.sql",
        "912ee1cb3e39f11aea5ce74c01103056ba02afee24f35d50f6cb0f166c31f45d",
    ),
    (
        "migrations/20260813000001_tool_call_diff.sql",
        "6399549ff613fc89a3bb017396a98ab45a2c06be7ba175030f86d610b38d5c90",
    ),
    (
        "migrations/20260813000002_sandbox_snapshots.sql",
        "c6a61accd81993e7f90d911b0fa58f47ecd38e1ef96ed5c987418bcc65047a29",
    ),
    (
        "migrations/20260813000003_snapshots_per_scope.sql",
        "290f00a2e26fc569b72291d4f8bbdd5ce882758903c877ad5ead14d0bc3a70c0",
    ),
    (
        "migrations/20260814000001_session_runtime.sql",
        "14efb26bea364e1981dce75b3e17616f5b1d3209f4d2188ba7c5142b30583112",
    ),
    (
        "migrations-global/20260810000002_auth_schema.sql",
        "9f9ea8e13b5a3873f8d0f6ec84651fa08288ef074cdb2a25cce930425a71ef32",
    ),
];

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/cortex-store
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("cortex-store should sit two levels below the repository root")
        .to_path_buf()
}

#[test]
fn applied_migrations_are_byte_for_byte_unchanged() {
    let root = repo_root();
    let mut wrong = Vec::new();
    for (rel, want) in PINNED {
        let path = root.join(rel);
        let bytes =
            std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let got = sha256_hex(&bytes);
        if got != *want {
            wrong.push(format!(
                "  {rel}
    pinned {want}
    actual {got}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "an applied migration changed on disk:
{}

sqlx hashes the whole file,          comments and all, so this is not cosmetic: the next deployment will stop with          `was previously applied but has been modified` and leave the schema half-migrated.          Put the correction in a new migration instead.",
        wrong.join("
")
    );
}

#[test]
fn every_migration_on_disk_is_pinned() {
    let root = repo_root();
    let pinned: BTreeSet<&str> = PINNED.iter().map(|(p, _)| *p).collect();
    let mut unpinned = Vec::new();
    for dir in ["migrations", "migrations-global"] {
        let entries =
            std::fs::read_dir(root.join(dir)).unwrap_or_else(|e| panic!("cannot list {dir}: {e}"));
        for entry in entries {
            let name = entry.expect("readable directory entry").file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".sql") {
                continue;
            }
            let rel = format!("{dir}/{name}");
            if !pinned.contains(rel.as_str()) {
                unpinned.push(rel);
            }
        }
    }
    assert!(
        unpinned.is_empty(),
        "these migrations are not pinned:
  {}

Add each one to PINNED with its          sha256. Doing that by hand is deliberate -- it is the moment to check that you          are adding a file rather than editing an applied one.",
        unpinned.join("
  ")
    );
}
