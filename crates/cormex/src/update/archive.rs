//! Is this the archive the release signed, and what is inside it.
//!
//! # Why the checksum is checked against `SHA256SUMS` and not a signature
//!
//! `SHA256SUMS` comes from the same release as the archive, so it does not
//! defend against a compromised release — the same reasoning `scripts/install.sh`
//! gives. What it does defend against is the failure that actually happens: a
//! proxy that truncates, a mirror that serves a stale object, a download that
//! ended early on a flaky link. Replacing a working binary with half of a new
//! one is unrecoverable from inside the tool, so the check runs before anything
//! is written.
//!
//! # Why the payload is checked for an executable header
//!
//! The archive's layout is a convention between `release.yml` and this file: a
//! directory named `cormex-<version>-<target>` holding `cormex`, `README.md`
//! and `LICENSE`. Picking "the entry whose name matches" already fails if the
//! convention drifts, but a silently-wrong pick that happens to be a file is
//! worse than a loud one — so the bytes also have to start with the magic
//! number of an executable for this platform.

use std::io::Read as _;

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

/// How a release archive is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    /// `.tar.gz` — Linux and macOS.
    TarGz,
    /// `.zip` — Windows, because Explorer opens it without help.
    Zip,
}

impl Archive {
    /// The extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }
}

/// The digest `SHA256SUMS` lists for `asset`.
///
/// The file is `sha256sum *.tar.gz *.zip` output: one `<hex>  <name>` line per
/// archive, six of them. Only the line for **this** asset matters — the other
/// five are binaries for platforms that are not here, and `sha256sum -c` over
/// the whole list would fail on every one of them.
///
/// # Errors
/// No line names `asset`, or the hex column is not a SHA-256 digest.
pub fn expected_digest(sums: &str, asset: &str) -> Result<String> {
    let mut listed = Vec::new();
    for line in sums.lines() {
        let mut columns = line.split_whitespace();
        let (Some(digest), Some(name)) = (columns.next(), columns.next()) else {
            continue;
        };
        // `sha256sum -b` writes `abc123 *cormex-….tar.gz`; the asterisk is a
        // mode marker, not part of the name
        let name = name.trim_start_matches('*');
        listed.push(name.to_owned());
        if name == asset {
            anyhow::ensure!(
                digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
                "SHA256SUMS lists {asset} with {digest:?}, which is not a SHA-256 digest"
            );
            return Ok(digest.to_ascii_lowercase());
        }
    }
    anyhow::bail!(
        "SHA256SUMS does not list {asset}, so the download cannot be verified. \
         It lists: {}",
        if listed.is_empty() {
            "nothing".to_owned()
        } else {
            listed.join(", ")
        }
    )
}

/// Compare `bytes` against the digest the release published.
///
/// # Errors
/// The digest does not match. The message carries both digests: "checksum
/// mismatch" alone leaves the user unable to tell a corrupt download from a
/// broken release.
pub fn verify(bytes: &[u8], expected: &str, asset: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    anyhow::ensure!(
        actual.eq_ignore_ascii_case(expected.trim()),
        "{asset} does not match SHA256SUMS\n  expected {expected}\n  actual   {actual}\n\
         The download is corrupt. Try again; if it persists, the release is broken and this \
         binary was left untouched."
    );
    Ok(())
}

/// Pull the binary out of a release archive.
///
/// # Errors
/// The archive does not decompress, does not hold a file named `exe`, or what
/// it holds is not an executable for this platform.
pub fn binary(archive: Archive, bytes: &[u8], exe: &str) -> Result<Vec<u8>> {
    let binary = match archive {
        Archive::TarGz => binary_from_tar_gz(bytes, exe)?,
        Archive::Zip => binary_from_zip(bytes, exe)?,
    };
    check_executable(&binary, archive, exe)?;
    anyhow::ensure!(
        binary.len() > 4096,
        "the archive's {exe} is {} bytes, which is far too small to be the CLI",
        binary.len()
    );
    Ok(binary)
}

fn binary_from_tar_gz(bytes: &[u8], exe: &str) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = tar
        .entries()
        .context("the archive is not a gzip-compressed tar")?;
    for entry in entries {
        let mut entry = entry.context("the archive has a damaged member")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        // The name at the end of the entry's path, not the whole path: the
        // archive wraps everything in `cormex-<version>-<target>/`, and matching
        // that would tie this code to the version it happens to be updating to
        let Ok(path) = entry.path() else {
            continue;
        };
        if path.to_str().map(archive_file_name) != Some(exe) {
            continue;
        }
        let mut out = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut out)
            .context("the archive's binary is truncated")?;
        return Ok(out);
    }
    anyhow::bail!(
        "the archive holds no file named {exe}. It was built for a different platform, or the \
         release changed its layout"
    )
}

/// The last segment of an archive entry's path.
///
/// Both separators, on purpose. `release.yml` packages Windows with `zip` when
/// the runner has it and falls back to PowerShell's `Compress-Archive` when it
/// does not — and that one writes `cormex-0.1.13-x86_64-pc-windows-msvc\cormex.exe`,
/// a backslash inside a zip entry name. It is non-standard and every reader
/// accepts it, but `std::path::Path::file_name` on Unix treats the backslash as
/// an ordinary character, so the split has to be done here rather than through
/// `Path`.
fn archive_file_name(entry: &str) -> &str {
    entry.rsplit(['/', '\\']).next().unwrap_or(entry)
}

#[cfg(windows)]
fn binary_from_zip(bytes: &[u8], exe: &str) -> Result<Vec<u8>> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("the archive is not a zip")?;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index)?;
        if !file.is_file() {
            continue;
        }
        if archive_file_name(file.name()) != exe {
            continue;
        }
        let mut out = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
        file.read_to_end(&mut out)
            .context("the archive's binary is truncated")?;
        return Ok(out);
    }
    anyhow::bail!(
        "the archive holds no file named {exe}. It was built for a different platform, or the \
         release changed its layout"
    )
}

#[cfg(not(windows))]
fn binary_from_zip(_bytes: &[u8], _exe: &str) -> Result<Vec<u8>> {
    // `release.yml` ships a zip only for Windows, and `target::archive` only
    // asks for one there, so reaching this means a hand-made call
    anyhow::bail!("zip archives are only published for Windows")
}

/// The payload starts with an executable magic number.
///
/// The four checked are the ones the six release targets can produce: ELF
/// (musl), Mach-O 64-bit and the universal/fat wrapper, and PE.
fn check_executable(bytes: &[u8], archive: Archive, exe: &str) -> Result<()> {
    const ELF: &[u8] = b"\x7fELF";
    const MACHO_64: &[u8] = &[0xcf, 0xfa, 0xed, 0xfe];
    const MACHO_FAT: &[u8] = &[0xca, 0xfe, 0xba, 0xbe];
    const PE: &[u8] = b"MZ";
    let looks_right = bytes.starts_with(ELF)
        || bytes.starts_with(MACHO_64)
        || bytes.starts_with(MACHO_FAT)
        || bytes.starts_with(PE);
    anyhow::ensure!(
        looks_right,
        "the {} archive's {exe} is not an executable (it starts with {:02x?}); nothing was \
         replaced",
        archive.extension(),
        &bytes[..bytes.len().min(4)]
    );
    Ok(())
}

/// Builders for the shapes the release pipeline produces.
///
/// Shared with the end-to-end test in the parent module, which needs a real
/// archive to serve: assembling one in two places would let the two disagree
/// about what a release looks like.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::io::Write as _;

    /// Payload with the magic number of an executable and enough size to pass.
    pub fn fake_binary() -> Vec<u8> {
        let mut bytes = if cfg!(windows) {
            b"MZ".to_vec()
        } else {
            b"\x7fELF\x02\x01\x01\x00".to_vec()
        };
        bytes.resize(16384, 0x90);
        bytes
    }

    /// A `tar.gz` with the release's layout: a wrapper directory holding files.
    pub fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *body)
                .expect("append");
        }
        builder.into_inner().expect("finish").finish().expect("gz")
    }

    /// A `zip` with the release's layout.
    ///
    /// Entries are written with **backslashes**, which is what the published
    /// Windows archive actually contains: `release.yml` falls back to
    /// PowerShell's `Compress-Archive`, and that writes them that way. A fixture
    /// with forward slashes would pass while the real archive failed.
    #[cfg(windows)]
    pub fn zipfile(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, body) in entries {
            writer
                .start_file::<_, ()>(
                    path.replace('/', "\\"),
                    zip::write::SimpleFileOptions::default(),
                )
                .expect("start");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish").into_inner()
    }

    /// The archive a release would publish for this platform.
    pub fn release_archive(binary: &[u8]) -> Vec<u8> {
        let dir = format!(
            "cormex-9.9.9-{}",
            crate::update::target::current_target().expect("a released platform")
        );
        let exe = crate::update::target::exe_name();
        let readme = format!("{dir}/README.md");
        let license = format!("{dir}/LICENSE");
        let exe_path = format!("{dir}/{exe}");
        let entries: [(&str, &[u8]); 3] = [
            (readme.as_str(), b"# cormex".as_slice()),
            (license.as_str(), b"Apache-2.0".as_slice()),
            (exe_path.as_str(), binary),
        ];
        #[cfg(windows)]
        return zipfile(&entries);
        #[cfg(not(windows))]
        tarball(&entries)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{fake_binary, tarball};
    use super::*;

    /// What the release's `SHA256SUMS` looks like: six archives and nothing else.
    fn sums(asset: &str, digest: &str) -> String {
        format!(
            "aaaa0000000000000000000000000000000000000000000000000000000000aa  \
             cormex-0.1.14-aarch64-apple-darwin.tar.gz\n\
             {digest}  {asset}\n\
             bbbb0000000000000000000000000000000000000000000000000000000000bb  \
             cormex-0.1.14-x86_64-pc-windows-msvc.zip\n"
        )
    }

    #[test]
    fn the_digest_is_the_line_for_this_asset() {
        let asset = "cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz";
        let digest = "c0ffee00000000000000000000000000000000000000000000000000000000ff";
        assert_eq!(
            expected_digest(&sums(asset, digest), asset).expect("listed"),
            digest
        );
    }

    #[test]
    fn a_digest_is_matched_by_exact_name() {
        // A sibling platform's line must not be mistaken for ours: five of the
        // six names differ only in their tail
        let sums = sums(
            "cormex-0.1.14-aarch64-unknown-linux-musl.tar.gz",
            "dddd0000000000000000000000000000000000000000000000000000000000dd",
        );
        let err = expected_digest(&sums, "cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz")
            .expect_err("only the aarch64 asset is listed");
        assert!(
            err.to_string().contains("aarch64-unknown-linux-musl"),
            "{err}"
        );
    }

    #[test]
    fn an_unlisted_archive_says_what_is_listed() {
        let err = expected_digest("", "cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz")
            .expect_err("an empty file lists nothing");
        assert!(err.to_string().contains("nothing"), "{err}");
    }

    #[test]
    fn a_binary_mode_checksum_line_still_matches() {
        // `sha256sum -b` writes an asterisk before the name
        let digest = "eeee0000000000000000000000000000000000000000000000000000000000ee";
        let text = format!("{digest} *cormex-0.1.14-x86_64-apple-darwin.tar.gz\n");
        assert_eq!(
            expected_digest(&text, "cormex-0.1.14-x86_64-apple-darwin.tar.gz").unwrap(),
            digest
        );
    }

    #[test]
    fn a_short_digest_is_refused_rather_than_compared() {
        let err = expected_digest("abc123  a.tar.gz\n", "a.tar.gz")
            .expect_err("a digest that is not 32 bytes is corrupt");
        assert!(err.to_string().contains("not a SHA-256"), "{err}");
    }

    #[test]
    fn a_mismatch_names_both_digests() {
        let err = verify(b"not the archive", &"0".repeat(64), "cormex-1.tar.gz")
            .expect_err("different bytes cannot match");
        let text = err.to_string();
        assert!(text.contains("cormex-1.tar.gz"), "{text}");
        assert!(text.contains(&"0".repeat(64)), "{text}");
        assert!(text.contains("actual"), "{text}");
    }

    #[test]
    fn the_binary_comes_out_of_the_tarball_with_its_readme_left_behind() {
        let payload = fake_binary();
        let archive = tarball(&[
            ("cormex-0.1.14-x86_64-unknown-linux-musl/README.md", b"# hi"),
            ("cormex-0.1.14-x86_64-unknown-linux-musl/LICENSE", b"Apache"),
            ("cormex-0.1.14-x86_64-unknown-linux-musl/cormex", &payload),
        ]);
        let extracted = binary(Archive::TarGz, &archive, "cormex").expect("the binary is there");
        assert_eq!(extracted, payload, "the payload must be byte for byte");
    }

    #[test]
    fn a_tarball_without_the_binary_is_refused() {
        let archive = tarball(&[("cormex-0.1.14-x86_64-unknown-linux-musl/README.md", b"# hi")]);
        let err = binary(Archive::TarGz, &archive, "cormex").expect_err("no binary in there");
        assert!(err.to_string().contains("no file named cormex"), "{err}");
    }

    #[test]
    fn a_payload_that_is_not_an_executable_is_refused() {
        let archive = tarball(&[("pkg/cormex", b"#!/bin/sh\necho hi\n")]);
        let err = binary(Archive::TarGz, &archive, "cormex").expect_err("a shell script");
        assert!(err.to_string().contains("not an executable"), "{err}");
        assert!(err.to_string().contains("nothing was replaced"), "{err}");
    }

    #[test]
    fn a_truncated_gzip_is_an_error_not_a_panic() {
        let mut archive = tarball(&[("pkg/cormex", &fake_binary())]);
        archive.truncate(archive.len() / 2);
        assert!(binary(Archive::TarGz, &archive, "cormex").is_err());
    }

    #[test]
    fn a_tiny_payload_is_refused_even_with_the_right_magic() {
        let archive = tarball(&[("pkg/cormex", b"\x7fELF\x02\x01")]);
        let err = binary(Archive::TarGz, &archive, "cormex").expect_err("8 bytes is not a CLI");
        assert!(err.to_string().contains("far too small"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn the_binary_comes_out_of_the_zip() {
        let payload = fake_binary();
        let bytes = super::fixtures::zipfile(&[
            ("cormex-9.9.9-x86_64-pc-windows-msvc/README.md", b"# hi"),
            ("cormex-9.9.9-x86_64-pc-windows-msvc/cormex.exe", &payload),
        ]);
        let extracted = binary(Archive::Zip, &bytes, "cormex.exe").expect("the exe is there");
        assert_eq!(extracted, payload);
    }

    /// The 0.1.13 Windows archive really is laid out this way.
    ///
    /// `release.yml` prefers `zip` and falls back to PowerShell's
    /// `Compress-Archive`, which writes backslashes inside entry names. The
    /// first version of this code matched on `/` alone and worked on every
    /// hand-made fixture while failing on the actual release — the whole
    /// command was dead on Windows and the unit tests were green.
    #[test]
    fn a_backslash_in_an_entry_name_still_finds_the_binary() {
        assert_eq!(
            archive_file_name("cormex-0.1.13-x86_64-pc-windows-msvc\\cormex.exe"),
            "cormex.exe"
        );
        assert_eq!(
            archive_file_name("cormex-0.1.13-x86_64-pc-windows-msvc/cormex"),
            "cormex"
        );
        assert_eq!(archive_file_name("cormex"), "cormex");
    }
}
