//! Where the device key lives.
//!
//! Design: docs/design/session-archive-architecture.md §5.6.
//!
//! # Three sources, in a fixed order
//!
//! 1. `CORMEX_DEVICE_KEY` — the escape hatch for containers and CI, where there
//!    is no keyring and writing a file would mean baking a secret into an image
//! 2. the system keyring — the right place on a desktop
//! 3. `<config_dir>/credentials`, mode 0600 — the fallback for a headless
//!    machine, and `doctor` says so
//!
//! # Why the fallback file is not a compromise to be ashamed of
//!
//! A headless server has no keyring, and refusing to work there would mean the
//! product only works on laptops. What matters is that the file is 0600 and
//! that the user is **told** which of the three is in use — a credential in a
//! file the user does not know about is the actual problem, not the file.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// The environment variable that overrides everything.
pub const DEVICE_KEY_ENV: &str = "CORMEX_DEVICE_KEY";

/// The keyring service name. Matches the product name, so a user looking in
/// their keychain finds it under the name they installed.
const KEYRING_SERVICE: &str = "cormex";

/// Which of the three sources answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    Keyring,
    File,
}

impl CredentialSource {
    /// One phrase for `status` and `doctor`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment (CORMEX_DEVICE_KEY)",
            Self::Keyring => "system keyring",
            Self::File => "file (0600)",
        }
    }
}

/// Read the device key, and say where it came from.
///
/// # Errors
/// None of the three sources has a key.
pub fn load(config_dir: &Path, server_url: &str) -> Result<(String, CredentialSource)> {
    if let Ok(key) = std::env::var(DEVICE_KEY_ENV) {
        let key = key.trim().to_owned();
        if !key.is_empty() {
            return Ok((key, CredentialSource::Environment));
        }
    }

    if let Some(key) = keyring_get(server_url) {
        return Ok((key, CredentialSource::Keyring));
    }

    let path = file_path(config_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let key = text.trim().to_owned();
            if key.is_empty() {
                anyhow::bail!("{} exists but is empty", path.display());
            }
            Ok((key, CredentialSource::File))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "no device key for {server_url}. Run `cormex login`, or set {DEVICE_KEY_ENV} if \
             this machine has no keyring"
        ),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Store a key, preferring the keyring.
///
/// Returns where it actually landed. A keyring failure is **not** fatal: a
/// headless machine has none, and refusing to store the key there would mean
/// `cormex login` cannot complete at all.
///
/// # Errors
/// Neither the keyring nor the file could be written.
pub fn store(config_dir: &Path, server_url: &str, key: &str) -> Result<CredentialSource> {
    if keyring_set(server_url, key) {
        // Drop any stale file copy: leaving both means the next read may pick
        // the older one, and "I logged in again and it still uses the old key"
        // has no visible cause
        let _ = std::fs::remove_file(file_path(config_dir));
        return Ok(CredentialSource::Keyring);
    }
    write_file(config_dir, key)?;
    Ok(CredentialSource::File)
}

/// Remove the key from wherever it is.
///
/// # Errors
/// Neither source could be cleared.
pub fn clear(config_dir: &Path, server_url: &str) -> Result<()> {
    keyring_delete(server_url);
    match std::fs::remove_file(file_path(config_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("could not remove the credentials file"),
    }
}

fn file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("credentials")
}

fn write_file(config_dir: &Path, key: &str) -> Result<()> {
    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("could not create {}", config_dir.display()))?;
    let path = file_path(config_dir);
    std::fs::write(&path, key).with_context(|| format!("could not write {}", path.display()))?;
    restrict_permissions(&path)?;
    Ok(())
}

/// `chmod 0600` on Unix.
///
/// On Windows the file inherits the user profile's ACL, which is already
/// per-user; there is no equivalent call and none is needed.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict the permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn keyring_get(server_url: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, server_url).ok()?;
    entry.get_password().ok().filter(|k| !k.trim().is_empty())
}

fn keyring_set(server_url: &str, key: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, server_url)
        .and_then(|e| e.set_password(key))
        .is_ok()
}

fn keyring_delete(server_url: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, server_url) {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file fallback is 0600 on Unix.
    ///
    /// This is the whole reason the fallback is acceptable: a world-readable
    /// file holding a device key is a credential leak on a shared machine, and
    /// nothing about it would be visible to the user.
    #[cfg(unix)]
    #[test]
    fn the_fallback_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("a temp dir");
        // Bypass `store` so the keyring (which may exist on this machine) does
        // not take the key instead
        write_file(dir.path(), "a-device-key").expect("write");
        let mode = std::fs::metadata(dir.path().join("credentials"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the credentials file is {mode:o}; a device key readable by other users on a shared \
             machine is a leak with no visible symptom"
        );
    }

    /// The environment variable wins, a missing key names the fix, and an empty
    /// file is an error rather than an empty key.
    ///
    /// **One test function, not three**: `set_var` is process-global, and three
    /// separate `#[test]`s run in parallel — the environment case would leak
    /// into the other two and make them fail for a reason that has nothing to
    /// do with what they check. The same shape as `reembed.rs`'s switch test.
    #[test]
    fn the_three_sources_behave_as_documented() {
        let dir = tempfile::tempdir().expect("a temp dir");

        // 1. An empty file is an error, not an empty key: an empty key goes out
        //    as `Authorization: Bearer ` and comes back 401, which reads like
        //    "your key expired"
        std::fs::write(dir.path().join("credentials"), "   \n").expect("write");
        let err = load(dir.path(), "https://never-configured.invalid")
            .expect_err("an empty file is not a key");
        assert!(err.to_string().contains("empty"), "{err}");
        std::fs::remove_file(dir.path().join("credentials")).expect("remove");

        // 2. With nothing configured, the error names the fix
        let err = load(dir.path(), "https://never-configured.invalid")
            .expect_err("there should be no key");
        assert!(
            err.to_string().contains("cormex login"),
            "the error should name the command that fixes it, got: {err}"
        );

        // 3. The environment variable wins over everything. It is the
        //    container/CI path, where there is no keyring and the file may be a
        //    read-only mount
        // SAFETY: this is the only test in this file that touches this
        // variable, and it removes it again
        unsafe { std::env::set_var(DEVICE_KEY_ENV, "from-env") };
        let (key, source) = load(dir.path(), "https://example").expect("load");
        assert_eq!(key, "from-env");
        assert_eq!(source, CredentialSource::Environment);
        unsafe { std::env::remove_var(DEVICE_KEY_ENV) };
    }
}
