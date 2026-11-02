//! Putting the downloaded binary in place of this one.
//!
//! # Why the new binary is staged beside the old one
//!
//! It is run **before** anything is replaced, so a download that is subtly
//! wrong (truncated in a way the checksum missed, built for the wrong libc,
//! quarantined by the OS) is discovered while the current binary is still
//! intact. Running it from `std::env::temp_dir()` would not do: `/tmp` is
//! `noexec` on a number of hardened systems, and "the new binary could not be
//! executed" would then look like a broken release rather than a mount option.
//! Staging beside the destination also guarantees the final `rename` stays on
//! one filesystem, which is what makes it atomic.
//!
//! # Why Windows needs a detour
//!
//! A running executable cannot be overwritten on Windows — `rename` onto its
//! own path fails with "access denied". It **can** be renamed aside, so the
//! old binary moves to `<name>.old`, the staged file takes its place, and the
//! leftover is deleted on the next run (`sweep`) or by whatever cleans
//! `%TEMP%`. Unix needs none of this: `rename` over the path is atomic and the
//! running process keeps the old inode.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::release::Version;

/// Refuse the two installations a self-replacement would damage.
///
/// # Errors
/// The binary came from npm, or is a development build, and `force` is off.
pub fn guard(dest: &Path, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    // Both checks look at the **whole** path, normalised to forward slashes,
    // so that `C:\…\node_modules\…` and `/…/node_modules/…` are one rule
    let text = dest.to_string_lossy().replace('\\', "/");
    if text.contains("/node_modules/") {
        anyhow::bail!(
            "cormex was installed by npm ({}), and overwriting the file leaves npm believing it \
             manages a version that is no longer there -- the next `npm install -g cormex` \
             either reinstalls the old one or fails on a changed checksum. Run:\n  \
             npm install -g cormex@latest\n\
             or pass --force to overwrite it anyway",
            dest.display()
        );
    }
    if text.contains("/target/debug/") {
        anyhow::bail!(
            "{} is a development build. `cormex update` would trade a working tree for a \
             release binary; rebuild instead (`cargo build -p cormex`), or pass --force",
            dest.display()
        );
    }
    Ok(())
}

/// Delete the copy a previous Windows update had to leave behind.
///
/// Best effort: on the run that performed the swap the old binary was still
/// executing, so it could not be deleted; the next run can. Failure is not an
/// error — a locked file in a directory the user can write to costs 6 MB, and
/// reporting it would make a successful update look like a failed one.
pub fn sweep(dest: &Path) {
    let _ = std::fs::remove_file(aside_path(dest));
}

/// The path `commit` moves the old binary to on Windows.
fn aside_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    dest.with_file_name(name)
}

/// The path the new binary is written to before it is committed.
fn staging_path(dest: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(dest.file_name().unwrap_or_default());
    name.push(".new");
    dest.with_file_name(name)
}

/// A downloaded binary, written next to its destination and not yet in place.
///
/// Dropping this without calling [`Staged::commit`] deletes the staged file,
/// which is what makes "every check failed, nothing changed" true by
/// construction rather than by remembering to clean up.
pub struct Staged {
    staged: PathBuf,
    dest: PathBuf,
}

impl Staged {
    /// Where the staged file is, for messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.staged
    }

    /// Run the staged binary and check that it reports `want`.
    ///
    /// # Errors
    /// It does not run, exits non-zero, or prints a different version.
    pub fn smoke_test(&self, want: Version) -> Result<()> {
        let output = std::process::Command::new(&self.staged)
            .arg("--version")
            .output()
            .with_context(|| {
                format!(
                    "the new binary could not be run ({}); nothing was replaced",
                    self.staged.display()
                )
            })?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            output.status.success(),
            "the new binary exited with {} when asked for its version:\n{}",
            output.status,
            text.trim()
        );
        verify_version_output(&text, want)
    }

    /// Replace the destination.
    ///
    /// # Errors
    /// The rename fails. On Windows the old binary is put back before the error
    /// is returned, so a half-applied update cannot be left behind.
    pub fn commit(self) -> Result<()> {
        swap(&self.staged, &self.dest)?;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Gone after a successful commit; present after any failure. Either
        // way, removing it is the right thing and NotFound is not an error
        let _ = std::fs::remove_file(&self.staged);
    }
}

/// Write `binary` beside `dest`, ready to be run and then committed.
///
/// # Errors
/// The directory is not writable, or the file cannot be written.
pub fn stage(binary: &[u8], dest: &Path) -> Result<Staged> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", dest.display()))?;
    let staged = staging_path(dest);
    let mut file = std::fs::File::create(&staged).map_err(|e| write_failed(e, dir))?;
    file.write_all(binary)
        .map_err(|e| write_failed(e, dir))
        .with_context(|| format!("could not write {}", staged.display()))?;
    // Without this, a machine that loses power between the write and the
    // rename comes back with a zero-length `cormex`, which is worse than
    // either version
    file.sync_all().context("could not flush the new binary")?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // The download inherits nothing: the temp file is created 0644 by the
        // umask, and an update that leaves the CLI unexecutable is a machine
        // with no CLI
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| write_failed(e, dir))?;
    }

    Ok(Staged {
        staged,
        dest: dest.to_path_buf(),
    })
}

#[cfg(unix)]
fn swap(staged: &Path, dest: &Path) -> Result<()> {
    std::fs::rename(staged, dest).map_err(|e| swap_failed(e, dest))
}

#[cfg(windows)]
fn swap(staged: &Path, dest: &Path) -> Result<()> {
    if !dest.exists() {
        return std::fs::rename(staged, dest).map_err(|e| swap_failed(e, dest));
    }
    let old = aside_path(dest);
    // A leftover from the previous update. It is deletable now if that process
    // has exited, and `rename` below replaces it if it is not
    let _ = std::fs::remove_file(&old);
    std::fs::rename(dest, &old).map_err(|e| swap_failed(e, dest))?;
    match std::fs::rename(staged, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(&old);
            Ok(())
        }
        Err(e) => {
            // Put the working binary back. Failing here means the destination
            // is gone, so say both things rather than only the second
            if let Err(back) = std::fs::rename(&old, dest) {
                anyhow::bail!(
                    "could not move the new binary into {} ({e}), and could not put the old one \
                     back ({back}). The previous binary is at {} -- rename it to {} to recover",
                    dest.display(),
                    old.display(),
                    dest.display()
                );
            }
            Err(swap_failed(e, dest))
        }
    }
}

/// Check `--version` output against the version that was installed.
///
/// Matched **per whitespace-separated token**, not by substring: `0.1.1` is a
/// substring of `0.1.14`, so a substring check would accept a binary that
/// reports the wrong version for one release in ten.
fn verify_version_output(text: &str, want: Version) -> Result<()> {
    let want = want.to_string();
    let found = text
        .split_whitespace()
        .any(|token| token.trim_start_matches('v') == want);
    anyhow::ensure!(
        found,
        "the new binary reports {:?}, not {want}; nothing was replaced",
        text.trim()
    );
    Ok(())
}

fn write_failed(e: std::io::Error, dir: &Path) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return anyhow::anyhow!(
            "cannot write to {}: permission denied. `cormex update` replaces the file it is \
             running from, so it needs write access to that directory -- re-run it under sudo, \
             or reinstall into a directory you own (`scripts/install.sh` uses ~/.local/bin and \
             needs no privileges)",
            dir.display()
        );
    }
    anyhow::anyhow!("could not write to {}: {e}", dir.display())
}

fn swap_failed(e: std::io::Error, dest: &Path) -> anyhow::Error {
    write_failed(e, dest.parent().unwrap_or(dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(marker: u8) -> Vec<u8> {
        let mut bytes = b"\x7fELF".to_vec();
        bytes.resize(8192, marker);
        bytes
    }

    #[test]
    fn the_staged_binary_replaces_the_old_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = dir.path().join(if cfg!(windows) {
            "cormex.exe"
        } else {
            "cormex"
        });
        std::fs::write(&dest, payload(0)).expect("the old binary");

        let staged = stage(&payload(1), &dest).expect("writable directory");
        assert!(
            staged.path().exists(),
            "the staged file has to exist before it is run"
        );
        assert_eq!(
            std::fs::read(&dest).expect("still there"),
            payload(0),
            "staging must not touch the binary in place"
        );
        staged.commit().expect("the rename works");

        assert_eq!(std::fs::read(&dest).expect("replaced"), payload(1));
        let installed = dest
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| *name != installed)
            .collect();
        assert!(
            leftovers.is_empty(),
            "a committed update leaves one file behind, not {leftovers:?}"
        );
    }

    #[test]
    fn dropping_a_staged_binary_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = dir.path().join("cormex");
        std::fs::write(&dest, payload(0)).expect("the old binary");
        {
            let _staged = stage(&payload(9), &dest).expect("writable directory");
            // `_staged` is dropped here, as it is when a check fails
        }
        assert_eq!(std::fs::read(&dest).unwrap(), payload(0));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "the failed attempt's staging file must be gone"
        );
    }

    #[test]
    fn a_version_is_matched_as_a_token_not_as_a_substring() {
        let text = "cormex 0.1.14\n";
        assert!(verify_version_output(text, Version::parse("0.1.14").unwrap()).is_ok());
        let err = verify_version_output(text, Version::parse("0.1.1").unwrap())
            .expect_err("0.1.1 is a substring of 0.1.14, and is still a different version");
        assert!(err.to_string().contains("nothing was replaced"), "{err}");
    }

    #[test]
    fn a_tag_prefixed_version_line_is_accepted() {
        assert!(verify_version_output("cormex v0.1.14", Version::parse("0.1.14").unwrap()).is_ok());
    }

    #[test]
    fn an_npm_install_is_refused_with_the_command_that_fixes_it() {
        let dest = if cfg!(windows) {
            Path::new(
                "C:\\Users\\a\\AppData\\Roaming\\npm\\node_modules\\@cormex\\cli-win32-x64\\cormex.exe",
            )
        } else {
            Path::new("/usr/local/lib/node_modules/@cormex/cli-linux-x64/cormex")
        };
        let err = guard(dest, false).expect_err("npm owns that file");
        assert!(
            err.to_string().contains("npm install -g cormex@latest"),
            "{err}"
        );
        assert!(guard(dest, true).is_ok(), "--force is the way through");
    }

    #[test]
    fn a_development_build_is_refused() {
        let dest = Path::new("/repo/target/debug/cormex");
        let err = guard(dest, false).expect_err("that is a working tree");
        assert!(err.to_string().contains("development build"), "{err}");
        assert!(guard(dest, true).is_ok());
    }

    #[test]
    fn an_ordinary_install_path_is_allowed() {
        for dest in [
            Path::new("/home/a/.local/bin/cormex"),
            Path::new("C:\\Users\\a\\AppData\\Local\\cormex\\bin\\cormex.exe"),
            Path::new("/opt/cormex/cormex"),
        ] {
            assert!(guard(dest, false).is_ok(), "{} was refused", dest.display());
        }
    }

    #[test]
    fn the_aside_path_keeps_the_extension_order_windows_expects() {
        // `cormex.exe` becomes `cormex.exe.old`, not `cormex.old`
        assert_eq!(
            aside_path(Path::new("/x/cormex.exe")),
            PathBuf::from("/x/cormex.exe.old")
        );
        assert_eq!(
            staging_path(Path::new("/x/cormex")),
            PathBuf::from("/x/.cormex.new")
        );
    }

    /// The leftover from an update that replaced the running binary.
    ///
    /// It could not be deleted at the time — Windows will not unlink a running
    /// image — so the next run has to, or every update leaves another 10 MB
    /// beside the binary.
    #[test]
    fn a_leftover_binary_is_swept_away_and_a_missing_one_is_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = dir.path().join("cormex.exe");
        std::fs::write(&dest, b"current").expect("the binary");
        std::fs::write(aside_path(&dest), b"the previous one").expect("the leftover");

        sweep(&dest);
        assert!(
            !aside_path(&dest).exists(),
            "the previous binary has to go, or the directory grows by one binary per update"
        );
        assert!(dest.exists(), "sweeping must not touch the live binary");

        // Idempotent: the common case is that there is nothing to sweep
        sweep(&dest);
    }
}
