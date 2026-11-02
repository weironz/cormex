//! Which release archive belongs to *this* binary.
//!
//! # Why the platform is a compile-time answer
//!
//! The release publishes six archives, and their names carry a Rust target
//! triple — `cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz` and five
//! siblings. Only the binary knows which one is its own, and it knows it
//! because `--target` is what produced it. Asking `rustc -vV` at run time
//! would need a Rust toolchain installed on the machine running the updater,
//! which is precisely the machine that does not have one — the updater exists
//! for people who installed a binary.
//!
//! # Why Linux maps to musl even for a glibc build
//!
//! `release.yml` builds Linux with `cargo-zigbuild` against musl, so the
//! published Linux archive is a **statically linked** binary. A developer who
//! built the CLI locally has an `x86_64-unknown-linux-gnu` binary, and the musl
//! one runs on that same machine — refusing with "there is no
//! `…-linux-gnu` archive" would deny an update to the one platform that
//! happens to have two working builds.
//!
//! The triple strings are compared **byte for byte** with `release.yml`'s
//! matrix, which is the authority. Deriving them from the running platform
//! would let a typo produce a plausible-looking name that only 404s.

use anyhow::Result;

use super::archive::Archive;
use super::release::Version;

/// The binary's name inside the archive, and the word `--version` prints.
pub const BIN: &str = "cormex";

/// The release triple this binary was built for, if the release builds one.
///
/// `None` is a real answer: on a target outside the six (riscv64, FreeBSD, a
/// musl build on Windows) there is nothing to download, and saying so is
/// better than downloading a binary that cannot run.
#[must_use]
pub fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// The file name of the binary on this platform.
#[must_use]
pub const fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cormex.exe"
    } else {
        "cormex"
    }
}

/// The archive format the release uses on this platform.
///
/// Windows gets a `zip` and everything else a `tar.gz` because that is what
/// `release.yml` produces — `.zip` is the one format Explorer opens without
/// help, and `tar.gz` is the one `install.sh` can unpack with `tar` alone.
#[must_use]
pub const fn archive() -> Archive {
    if cfg!(windows) {
        Archive::Zip
    } else {
        Archive::TarGz
    }
}

/// The release asset that would replace this binary:
/// `cormex-0.1.14-<triple>.tar.gz`.
///
/// # Errors
/// This platform is outside the six the release builds.
pub fn asset_name(version: &Version) -> Result<String> {
    let target = require_target()?;
    Ok(format!(
        "{BIN}-{version}-{target}.{}",
        archive().extension()
    ))
}

/// The error for a platform the release does not build for.
///
/// It names the platform that was looked for and gives the from-source answer,
/// because the alternative reading of a bare "unsupported platform" is that the
/// updater is broken.
#[must_use]
pub fn no_build_for_this_platform() -> anyhow::Error {
    anyhow::anyhow!(
        "cormex publishes no binary for {}-{}, so there is nothing to update to. \
         The six released targets are x86_64/aarch64 × linux-musl, apple-darwin and \
         windows-msvc; build from source otherwise:\n  \
         cargo install --git https://github.com/weironz/cormex --locked cormex",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// [`current_target`], or the error to show instead.
///
/// # Errors
/// This platform is outside the six the release builds.
pub fn require_target() -> Result<&'static str> {
    match current_target() {
        Some(target) => Ok(target),
        None => Err(no_build_for_this_platform()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_name_matches_the_release_pipelines_layout() {
        // `release.yml` packages `dist/cormex-<version>-<target>/cormex` and
        // archives it as `cormex-<version>-<target>.tar.gz` (zip on Windows).
        // A name that drifts from that is a 404 at update time and nothing else
        let version = Version::parse("0.1.14").expect("a version");
        let name = asset_name(&version).expect("this test host is a released platform");
        assert!(
            name.starts_with("cormex-0.1.14-"),
            "the version must not carry the tag's leading `v`: {name}"
        );
        let target = current_target().expect("a released platform");
        assert!(
            name.starts_with(&format!("cormex-0.1.14-{target}.")),
            "the target triple has to be the whole middle of the name: {name}"
        );
        if cfg!(windows) {
            assert!(name.ends_with(".zip"), "Windows gets the zip: {name}");
        } else {
            assert!(name.ends_with(".tar.gz"), "Unix gets the tarball: {name}");
        }
    }

    #[test]
    fn the_exe_name_follows_the_platform() {
        assert_eq!(
            exe_name(),
            if cfg!(windows) {
                "cormex.exe"
            } else {
                "cormex"
            }
        );
    }

    #[test]
    fn an_unreleased_platform_is_named_in_the_error() {
        // Whatever this test runs on, the message has to name both the OS and
        // the architecture: "no build for you" without saying which build was
        // looked for is unactionable
        let text = no_build_for_this_platform().to_string();
        assert!(text.contains(std::env::consts::OS), "{text}");
        assert!(text.contains(std::env::consts::ARCH), "{text}");
        assert!(text.contains("cargo install"), "{text}");
    }

    #[test]
    fn every_published_triple_is_a_valid_answer() {
        // The six strings are copied from `release.yml`'s matrix. Pinning them
        // here means a rename on either side turns red instead of producing an
        // update that 404s on one platform only
        for target in [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            assert!(
                target.split('-').count() >= 3,
                "{target} is not a target triple"
            );
        }
        if let Some(target) = current_target() {
            assert_eq!(require_target().expect("a released platform"), target);
        }
    }
}
