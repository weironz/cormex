//! Verify credentials survive across separate CLI processes.

#[test]
fn child_reads_stored_credential() {
    let Ok(path) = std::env::var("CORMEX_CREDENTIAL_TEST_DIR") else {
        return;
    };
    let url = std::env::var("CORMEX_CREDENTIAL_TEST_URL").unwrap();
    let (key, _) = cormex::credentials::load(std::path::Path::new(&path), &url).unwrap();
    assert_eq!(key, "cormex-test-only-key");
}

#[test]
fn a_new_process_can_read_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("https://credential-test.invalid/{}", std::process::id());
    cormex::credentials::store(dir.path(), &url, "cormex-test-only-key").unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_reads_stored_credential"])
        .env("CORMEX_CREDENTIAL_TEST_DIR", dir.path())
        .env("CORMEX_CREDENTIAL_TEST_URL", &url)
        .env_remove("CORMEX_DEVICE_KEY")
        .status();
    cormex::credentials::clear(dir.path(), &url).unwrap();
    assert!(result.unwrap().success());
}
