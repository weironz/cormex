//! `cormex update` — replace this binary with a published release.
//!
//! # The order of the checks, which is the whole design
//!
//! 1. Ask the API what the newest release is, and refuse early if it publishes
//!    nothing for this platform.
//! 2. Look at the installation path and refuse the two cases where replacing
//!    the file is the wrong answer (npm, a development build).
//! 3. Download, and verify the archive against the release's `SHA256SUMS`.
//! 4. Extract, and check the payload is an executable for this platform.
//! 5. **Run it** — `--version` — while the old binary is still in place.
//! 6. Only then rename it over the destination, atomically.
//!
//! Every step before 6 can fail without touching the working binary, which is
//! the property that makes a self-updater safe to ship: there is no state in
//! which the machine has no CLI, and none in which it has a broken one.
//!
//! # Why the hooks and the scheduler survive an update
//!
//! `cormex init` records the **absolute path** of this binary in every hook and
//! in the scheduler entry (see `install::claude_code`). An update replaces the
//! file at that path, so those references keep pointing at something that
//! exists — there is nothing to re-run afterwards. Installing to a *different*
//! path would leave every host calling the old one, silently.

mod apply;
mod archive;
mod release;
mod target;

pub use self::release::Version;

use std::cmp::Ordering;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// What the user asked for.
#[derive(Debug, Clone)]
pub struct Options {
    /// Report what would happen, write nothing.
    pub check: bool,
    /// Install this tag (`v0.1.14`) instead of the newest release.
    pub tag: Option<String>,
    /// Install even when the version is the same or older, and past the
    /// guards that normally refuse.
    pub force: bool,
    /// The binary to replace. `None` means this one.
    pub current_exe: Option<PathBuf>,
    /// Run the new binary before committing it.
    ///
    /// **Only a test turns this off**, and only because a fake archive cannot
    /// contain a program that prints a version: the archive the test serves is
    /// ELF/MZ header plus padding. The real path always checks, because
    /// "downloads and installs, but does not start" is the failure that costs
    /// a user their CLI.
    pub smoke_test: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            check: false,
            tag: None,
            force: false,
            current_exe: None,
            smoke_test: true,
        }
    }
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Already at the newest version, or at a version that is not older.
    UpToDate,
    /// `--check` and a newer release exists. Nothing was written.
    Available,
    /// The binary was replaced.
    Installed { path: PathBuf },
}

/// The result, for the command to render.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The version this binary was before the run.
    pub current: Version,
    /// The version the release offers.
    pub target: Version,
    /// The asset that was (or would be) installed, by name.
    pub asset: String,
    /// Its size in bytes, as the API states it.
    pub bytes: u64,
    pub action: Action,
}

/// Check for, and install, the newest release.
///
/// # Errors
/// The release cannot be reached, publishes nothing for this platform, fails
/// its checksum, does not run, or cannot be written over the current binary.
pub async fn run(opts: &Options) -> Result<Outcome> {
    let source = release::Client::github()?;
    // Progress goes to stderr and the result to stdout, the same split `init`
    // uses: a script capturing the outcome should not have to filter a
    // download's narration out of it
    run_with(&source, opts, &mut |note| eprintln!("{note}")).await
}

/// [`run`] against a given API — the tests' entry point.
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    source: &release::Client,
    opts: &Options,
    note: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let current = Version::current();
    // Fails on a platform the release does not build for, before anything is
    // downloaded: without a triple there is no asset name to look for
    let triple = target::require_target()?;

    let release = match &opts.tag {
        Some(tag) => source.by_tag(&with_tag_prefix(tag)).await?,
        None => source.latest().await?,
    };
    let target_version = release.version().ok_or_else(|| {
        anyhow::anyhow!(
            "the release tag {:?} is not a version, so this binary cannot tell whether it is \
             newer. The release pipeline only publishes tags of the form vMAJOR.MINOR.PATCH",
            release.tag_name
        )
    })?;
    let asset = release
        .asset(&target::asset_name(&target_version)?)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} publishes no archive for this platform ({triple}); it has: {}",
                release.tag_name,
                release.asset_names()
            )
        })?;

    let mut outcome = Outcome {
        current,
        target: target_version,
        asset: asset.name.clone(),
        bytes: asset.size,
        action: Action::UpToDate,
    };

    if opts.check {
        if target_version > current {
            outcome.action = Action::Available;
        }
        return Ok(outcome);
    }

    match target_version.cmp(&current) {
        Ordering::Greater => {}
        Ordering::Equal if !opts.force => return Ok(outcome),
        Ordering::Equal => {}
        Ordering::Less if !opts.force => {
            anyhow::bail!(
                "release {target_version} is older than this binary ({current}). Pass --force to \
                 downgrade -- a downgrade can meet a state database written by the newer binary, \
                 which is not something the CLI migrates backwards"
            );
        }
        Ordering::Less => {}
    }

    let exe = match &opts.current_exe {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .context("could not work out where this binary is; pass the path explicitly")?,
    };
    // A leftover from an update that ran while its own file was locked
    apply::sweep(&exe);
    apply::guard(&exe, opts.force)?;

    note(&format!("downloading {}", asset.name));
    let sums = source.sums(&release).await?;
    let bytes = source.download(&asset).await?;
    let digest = archive::expected_digest(&sums, &asset.name)?;
    archive::verify(&bytes, &digest, &asset.name)?;
    let payload = archive::binary(target::archive(), &bytes, target::exe_name())?;

    let staged = apply::stage(&payload, &exe)?;
    if opts.smoke_test {
        note(&format!("running {} --version", staged.path().display()));
        staged.smoke_test(target_version)?;
    }
    staged.commit()?;

    outcome.action = Action::Installed { path: exe };
    Ok(outcome)
}

/// `0.1.14` → `v0.1.14`, because the tags carry the prefix and people type the
/// version without it.
fn with_tag_prefix(tag: &str) -> String {
    if tag.starts_with('v') {
        tag.to_owned()
    } else {
        format!("v{tag}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let address = listener.local_addr().expect("an address");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    struct Fake {
        base: String,
        archive: Vec<u8>,
        asset: String,
    }

    /// Stand a release in front of the updater: the API, the two assets, and
    /// the checksums file, at the paths GitHub would answer on.
    async fn fake_release(tag: &str, tamper_sum: bool) -> Fake {
        use axum::routing::get;

        let archive =
            super::archive::fixtures::release_archive(&super::archive::fixtures::fake_binary());
        let asset = target::asset_name(&Version::parse(tag).expect("a version"))
            .expect("this host is a released platform");
        let digest = if tamper_sum {
            "0".repeat(64)
        } else {
            format!("{:x}", sha2::Sha256::digest(&archive))
        };
        let sums = format!("{digest}  {asset}\n");
        let repo = "weironz/cormex".to_owned();
        let tagged = format!("v{tag}");

        // The URLs in the JSON have to point back at this server, so the port
        // is known before the routes are built
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let release = serde_json::json!({
            "tag_name": tagged,
            "assets": [
                {"name": "SHA256SUMS", "size": sums.len(),
                 "browser_download_url": format!("{base}/download/SHA256SUMS")},
                {"name": asset, "size": archive.len(),
                 "browser_download_url": format!("{base}/download/{asset}")},
            ]
        });

        // Each handler below clones what it serves: `Router` needs `'static`
        // handlers, and every one of these values is built after the base URL
        // is known
        let latest = release.clone();
        let by_tag = release.clone();
        let sums_body = sums.clone();
        let archive_body = archive.clone();
        let app = axum::Router::new()
            .route(
                &format!("/repos/{repo}/releases/latest"),
                get(move || {
                    let body = latest.clone();
                    async move { axum::Json(body) }
                }),
            )
            .route(
                &format!("/repos/{repo}/releases/tags/{tagged}"),
                get(move || {
                    let body = by_tag.clone();
                    async move { axum::Json(body) }
                }),
            )
            .route(
                "/download/SHA256SUMS",
                get(move || {
                    let body = sums_body.clone();
                    async move { body }
                }),
            )
            .route(
                &format!("/download/{asset}"),
                get(move || {
                    let body = archive_body.clone();
                    async move { body }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        Fake {
            base,
            archive,
            asset,
        }
    }

    fn client(base: &str) -> release::Client {
        release::Client::new(base, "weironz/cormex").expect("a usable client")
    }

    fn exe_in(dir: &std::path::Path) -> PathBuf {
        dir.join(target::exe_name())
    }

    /// The version the fixture publishes. Chosen above any version this crate
    /// could be, so the tests are not chasing the workspace's version number.
    const NEWER: &str = "9.9.9";

    #[tokio::test]
    async fn a_newer_release_is_installed_byte_for_byte() {
        let release = fake_release(NEWER, false).await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = exe_in(dir.path());
        std::fs::write(&dest, b"the old binary").expect("an old binary");

        let outcome = run_with(
            &client(&release.base),
            &Options {
                current_exe: Some(dest.clone()),
                smoke_test: false,
                ..Options::default()
            },
            &mut |_| {},
        )
        .await
        .expect("the release verifies and installs");

        assert_eq!(
            outcome.action,
            Action::Installed { path: dest.clone() },
            "the outcome has to say what it replaced"
        );
        assert_eq!(outcome.target, Version::parse(NEWER).unwrap());
        assert_eq!(
            std::fs::read(&dest).expect("installed"),
            super::archive::fixtures::fake_binary(),
            "the file in place must be the archive's payload, not the archive"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("readable").count(),
            1,
            "an update leaves exactly one file: the binary"
        );
    }

    /// The property worth testing above all others: a download that does not
    /// match its checksum must leave the working binary exactly as it was.
    #[tokio::test]
    async fn a_bad_checksum_changes_nothing() {
        let release = fake_release(NEWER, true).await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = exe_in(dir.path());
        std::fs::write(&dest, b"the old binary").expect("an old binary");

        let err = run_with(
            &client(&release.base),
            &Options {
                current_exe: Some(dest.clone()),
                smoke_test: false,
                ..Options::default()
            },
            &mut |_| {},
        )
        .await
        .expect_err("a digest of zeroes cannot match");

        assert!(
            err.to_string().contains("does not match SHA256SUMS"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&dest).expect("untouched"),
            b"the old binary",
            "a failed verification must not replace the binary"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("readable").count(),
            1,
            "a failed update must not leave its staging file behind"
        );
    }

    #[tokio::test]
    async fn check_reports_without_downloading() {
        let release = fake_release(NEWER, false).await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = exe_in(dir.path());
        std::fs::write(&dest, b"the old binary").expect("an old binary");

        let outcome = run_with(
            &client(&release.base),
            &Options {
                check: true,
                current_exe: Some(dest.clone()),
                ..Options::default()
            },
            &mut |_| {},
        )
        .await
        .expect("checking does not need write access");

        assert_eq!(outcome.action, Action::Available);
        assert_eq!(outcome.asset, release.asset);
        assert_eq!(outcome.bytes, release.archive.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), b"the old binary");
    }

    #[tokio::test]
    async fn a_pinned_older_release_needs_force() {
        let release = fake_release("0.0.1", false).await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let dest = exe_in(dir.path());
        std::fs::write(&dest, b"the old binary").expect("an old binary");

        let err = run_with(
            &client(&release.base),
            &Options {
                tag: Some("0.0.1".to_owned()),
                current_exe: Some(dest.clone()),
                smoke_test: false,
                ..Options::default()
            },
            &mut |_| {},
        )
        .await
        .expect_err("a downgrade has to be asked for");

        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"the old binary");
    }

    #[tokio::test]
    async fn an_api_that_has_no_release_is_reported_plainly() {
        let base = serve(axum::Router::new()).await;
        let err = run_with(&client(&base), &Options::default(), &mut |_| {})
            .await
            .expect_err("there is no release behind a 404");
        assert!(err.to_string().contains("404"), "{err}");
        assert!(
            err.to_string().contains("releases"),
            "the message should point at where releases live: {err}"
        );
    }

    #[tokio::test]
    async fn a_tag_without_its_v_is_accepted() {
        let release = fake_release(NEWER, false).await;
        let outcome = run_with(
            &client(&release.base),
            &Options {
                check: true,
                tag: Some(NEWER.to_owned()),
                ..Options::default()
            },
            &mut |_| {},
        )
        .await
        .expect("the tag is looked up with the prefix added");
        assert_eq!(outcome.target, Version::parse(NEWER).unwrap());
    }
}
