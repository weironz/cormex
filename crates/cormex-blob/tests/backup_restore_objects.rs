//! The object-store half of the backup/restore drill (P6-3).
//!
//! `backup_restore_drill.rs` covers the database. This covers the other half,
//! and it is the half that fails **silently**: a database restored without its
//! objects has every `capture_chunks` row pointing at bytes that are not there.
//! The session list is fine, the counts match, and every session reads as empty.
//!
//! # What is exercised
//!
//! The object store is a directory tree under a root. Backing it up is `tar` of
//! that tree; restoring is unpacking it somewhere else. What can go wrong is
//! **the layout**, not the copying — a restore that lands one directory too deep
//! produces a store whose `get_keyed` finds nothing, and nothing reports it.
//!
//! So this writes objects through `LocalFsBlobStore`, copies the tree the way
//! the operations doc says to, points a **second** store at the copy, and
//! asserts every object reads back byte for byte.
//!
//! # What it does not cover
//!
//! S3/RustFS itself. The production store is RustFS on a volume, and the drill
//! there is the same `tar` of the volume — but the volume's internal layout is
//! RustFS's business, and a test that reproduced it would be testing RustFS.
//! What this pins is the part that is ours: the key layout, and that a copy of
//! the tree is a working store.

use cormex_blob::{BlobStore as _, LocalFsBlobStore, TenantPrefix};

/// A tenant prefix for the drill.
fn tenant() -> TenantPrefix {
    TenantPrefix::new("public").expect("public is a legal tenant prefix")
}

/// Copy a directory tree, the way `tar` would.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create the destination");
    for entry in walkdir::WalkDir::new(from)
        .into_iter()
        .filter_map(Result::ok)
    {
        let rel = entry
            .path()
            .strip_prefix(from)
            .expect("every entry is under the root");
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

#[tokio::test]
async fn the_object_store_restores_byte_for_byte() {
    let original = tempfile::tempdir().expect("a temp dir");
    let restored = tempfile::tempdir().expect("a temp dir");

    let store = LocalFsBlobStore::new(original.path(), tenant())
        .await
        .expect("the local backend should come up");

    // Three objects under the capture layout, with different contents so a
    // mix-up between them would show
    let bodies: Vec<Vec<u8>> = vec![
        b"the first conversation".to_vec(),
        b"the second conversation, longer than the first".to_vec(),
        // A body large enough to span the store's internal buffering
        vec![0xAB; 200 * 1024],
    ];
    let mut keys = Vec::new();
    for body in &bodies {
        let sha = cormex_blob::hash::sha256_hex(body);
        let key = cormex_blob::hash::capture_key(&tenant(), &sha).expect("a capture key");
        store
            .put_keyed(&key, bytes::Bytes::from(body.clone()), "application/zstd")
            .await
            .expect("put");
        keys.push(key);
    }

    // ── back up: copy the tree ──
    copy_tree(original.path(), restored.path());

    // ── restore: a second store pointed at the copy ──
    let restored_store = LocalFsBlobStore::new(restored.path(), tenant())
        .await
        .expect("the restored backend should come up");

    for (key, body) in keys.iter().zip(&bodies) {
        let got = restored_store
            .get_keyed(key)
            .await
            .unwrap_or_else(|e| panic!("the restored store cannot read {key}: {e}"));
        assert_eq!(
            &got[..],
            &body[..],
            "the restored object at {key} does not match what was written"
        );
    }

    // And the count matches: a restore that silently dropped an object would
    // pass the per-key loop only if the key list were wrong too
    let files = walkdir::WalkDir::new(restored.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count();
    assert_eq!(
        files,
        bodies.len(),
        "the restored tree holds {files} files for {} objects",
        bodies.len()
    );
}

/// A restore that lands one directory too deep finds nothing.
///
/// This is the failure the drill exists to catch, and it is silent: the
/// database's row counts match, the session list is correct, and every session
/// reads as empty. Pinning it makes the shape of the mistake explicit.
#[tokio::test]
async fn a_restore_one_level_too_deep_finds_nothing() {
    let original = tempfile::tempdir().expect("a temp dir");
    let wrong = tempfile::tempdir().expect("a temp dir");

    let store = LocalFsBlobStore::new(original.path(), tenant())
        .await
        .expect("the local backend should come up");
    let body = b"a conversation".to_vec();
    let sha = cormex_blob::hash::sha256_hex(&body);
    let key = cormex_blob::hash::capture_key(&tenant(), &sha).expect("a key");
    store
        .put_keyed(&key, bytes::Bytes::from(body), "application/zstd")
        .await
        .expect("put");

    // Unpack into `wrong/nested` rather than `wrong` — the classic `tar -C`
    // mistake
    copy_tree(original.path(), &wrong.path().join("nested"));

    let restored = LocalFsBlobStore::new(wrong.path(), tenant())
        .await
        .expect("the backend should still come up: an empty store is legal");
    assert!(
        restored.get_keyed(&key).await.is_err(),
        "a store rooted one level above the data must not find the object -- if it does, this \
         test is not reproducing the mistake it claims to"
    );
}
