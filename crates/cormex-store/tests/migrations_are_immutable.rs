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
//!
//! # These hashes are of LF bytes, and `.gitattributes` is what makes that true
//!
//! The first version of this test pinned the hashes of a Windows working copy,
//! where git had handed out CRLF. Every one of them was wrong: the deployed
//! database records the LF bytes that a Linux CI checkout produced. An image
//! built from a CRLF working tree fails to migrate against a database that was
//! migrated from an LF one -- **with byte-identical content**, which is what
//! makes it so hard to read. `.gitattributes` pins `*.sql` to `eol=lf` so the
//! working tree matches the blob on every platform; these hashes assume it.

use std::collections::BTreeSet;

/// Every applied migration and the sha256 of its bytes.
const PINNED: &[(&str, &str)] = &[
    (
        "migrations/20260807000001_init.sql",
        "fc5db4761f4963f43f569fae05feb40a356ebab3865dea82a324b21881723047",
    ),
    (
        "migrations/20260807000002_session_events.sql",
        "8182768c03062d227c31b1383e3e9e3044a0387e3c65396ab548fd7f0db3be31",
    ),
    (
        "migrations/20260807000003_session_page_cursor.sql",
        "d275809569a5877a9b2400a24e830ad2840a9ae8d1b2f96843ef533fd21c477a",
    ),
    (
        "migrations/20260807000004_attachment_filename.sql",
        "e6dcbe789821eb0587afbe9cf360092dbdaa21714ea2521ee3cd7775618ce02f",
    ),
    (
        "migrations/20260807000005_episode_replay.sql",
        "62dccd542a6407f966d23008bb7cac41152f8be19254248b0ff68b33f33ca7a5",
    ),
    (
        "migrations/20260807000006_fact_provenance.sql",
        "5d3bc188c6bb51742f347f1424892da21a331300736a7bf7a130f6717fa1d7ad",
    ),
    (
        "migrations/20260808000001_embedding_backfill.sql",
        "a0db2ab2a7efaf1612fd9f52c5fff86105950c4d68039862cbbe09ff6d4a8b37",
    ),
    (
        "migrations/20260810000001_active_facts_uses_the_index.sql",
        "ecd298a1f32e829e4ec5df023c3d9fb5444c6b12e384baa1f70f26480b2252aa",
    ),
    (
        "migrations/20260812000001_byo_llm_key.sql",
        "e4c37ddfa93bc2b9d07ad9f70a70dc65fb14cd5cce0c795896e8b419ce60f857",
    ),
    (
        "migrations/20260812000002_byo_key_base_url.sql",
        "6af94b142e60ad608cef486aaaf01013e10a679ec26782b0c6e68d160ada1fe1",
    ),
    (
        "migrations/20260812000003_projects.sql",
        "4d35ffe5ba59339e7dba60a15b6655ed8b9d3ff25119331904de1f5df3040777",
    ),
    (
        "migrations/20260813000001_tool_call_diff.sql",
        "449b009830bbe9f9c1264a44126591c58999f8e95f98db27d713dc796bc9b281",
    ),
    (
        "migrations/20260813000002_sandbox_snapshots.sql",
        "8665bbdd8820e0556de84e14113b88ce73434a184123826e539143583a7afc8f",
    ),
    (
        "migrations/20260813000003_snapshots_per_scope.sql",
        "1d2581fa8fcec60c143cee4c2e984d315702bd158d79af486d0eb35499a4bc67",
    ),
    (
        "migrations/20260814000001_session_runtime.sql",
        "c225bc98cbcc8a24e53e974d4d04200ae8316472d2480f923652a9e7610b6a7f",
    ),
    (
        "migrations-global/20260810000002_auth_schema.sql",
        "3de8739b14fddac88f77a658cbde6af97f44a9389577627f28043f2401830fc0",
    ),
    (
        "migrations/20261101000001_capture_archive.sql",
        "d95e0c684c70f89c01b1dc2b786ee3bec2ab8648c6b0e080d4f4b229f5cdd8f0",
    ),
    (
        "migrations-global/20261101000001_device_keys.sql",
        "7febf586e0190fb79a00aa21fe74e63ae48d3fc37fcad0409880337170d63d36",
    ),
    (
        "migrations/20261115000001_transcript_projection.sql",
        "4af46e39693ac5b021c4cf190d95a207a4e7a25289a88ad1a00395cc3f3dac49",
    ),
    (
        "migrations/20261120000001_archive_index_events.sql",
        "7e32f1c9c91daba3e76d8c090af1cb526c9e4b6ab201d6fcabd264e0dfc16be2",
    ),
    (
        "migrations/20261125000001_derivation_transcript_source.sql",
        "45093c9a907f2a75d562396654831518df66264f7150d06387472f7460117a84",
    ),
    (
        "migrations/20261125000002_distill_progress.sql",
        "66c94d5c2fc7ff7695bac0df7ce842958b7a80103fc475556c1179169c4864ee",
    ),
    (
        "migrations/20261125000003_session_briefs.sql",
        "08ef74941cb50fd23d91789803099e2b82c0283c8d480e876ba64fa894f164d9",
    ),
    (
        "migrations/20261125000004_promote_progress.sql",
        "61c846fcfdc4c55bbd9fd68d29bdf911937336e2311a40e0ba9ace1c08ae3120",
    ),
    (
        "migrations/20261125000005_transcript_embeddings.sql",
        "30ab8c664a247d39bea817de114e9bbccc46b0de6f354d44712a5f00684340f6",
    ),
    (
        "migrations/20261125000006_capture_keys.sql",
        "28386dbbff62ac9728f4786b9c364d0e2149ab8ff38ea2d4495d0b7ec54b233d",
    ),
    (
        "migrations/20261125000007_redaction_capture_targets.sql",
        "8a401a04439d705826abfe366f79b92d9e73071b0597a24fa83df809935c2aaa",
    ),
];

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/cormex-store
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("cormex-store should sit two levels below the repository root")
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
