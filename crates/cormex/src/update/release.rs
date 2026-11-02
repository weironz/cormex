//! Releases, as GitHub publishes them.
//!
//! # Why the API and not the `/releases/latest` redirect
//!
//! `install.sh` reads the tag out of the redirect's `Location` because it has
//! to work with `curl` and nothing else. Here the JSON is the better source:
//! it carries the assets' names and URLs, so a mirror or a re-published release
//! works without the updater knowing how the archive is hosted, and the
//! checksums file is found by name rather than constructed.
//!
//! # Why the repository is a constant, not an environment variable
//!
//! An updater that can be pointed at another repository will eventually be
//! pointed at one whose releases are somebody else's. `Client::new` takes the
//! API base and the repository as arguments so the tests can stand a fake
//! release in front of it; nothing in the CLI reads them from the environment.

use std::fmt;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// The repository the CLI updates itself from.
pub const DEFAULT_REPO: &str = "weironz/cormex";

/// GitHub's API root.
pub const DEFAULT_API: &str = "https://api.github.com";

/// A release version: `0.1.13`.
///
/// # Why not the `semver` crate
///
/// A semver implementation is already in this build's dependency tree, so using
/// it would be free. It is still the wrong tool: `release.yml` refuses any tag
/// that is not `^v[0-9]+\.[0-9]+\.[0-9]+$`, so pre-release and build metadata
/// cannot occur in a tag this updater will ever see, and a full parser would
/// add states the binary can never be in — while making "0.2.0-rc1 sorts above
/// 0.2.0" a rule that has to be honoured for a release that cannot exist.
///
/// The comparison is **numeric per component**. `0.1.9` is older than `0.1.10`;
/// comparing the strings would say the opposite, and the symptom would be an
/// update offered forever on exactly one version in ten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `0.1.13` or `v0.1.13`, and nothing else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// The version this binary was built as.
    ///
    /// # Panics
    /// `CARGO_PKG_VERSION` is not a version. That is a broken build, not a
    /// runtime condition: the same string is what `--version` prints.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION"))
            .expect("CARGO_PKG_VERSION is always a three-part version")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One release.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// One file attached to a release.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
}

impl Release {
    /// The asset with this exact name.
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The tag as a version, for the caller to attach its own context to.
    #[must_use]
    pub fn version(&self) -> Option<Version> {
        Version::parse(&self.tag_name)
    }

    /// Every asset name, for the error that has to say what *is* available.
    #[must_use]
    pub fn asset_names(&self) -> String {
        if self.assets.is_empty() {
            return "nothing".to_owned();
        }
        self.assets
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Talks to one GitHub API.
pub struct Client {
    http: reqwest::Client,
    api_base: String,
    repo: String,
}

impl Client {
    /// The production client: `weironz/cormex` on `api.github.com`.
    ///
    /// # Errors
    /// The HTTP client cannot be built.
    pub fn github() -> Result<Self> {
        Self::new(DEFAULT_API, DEFAULT_REPO)
    }

    /// A client for any API root and repository — the tests' entry point.
    ///
    /// # Errors
    /// The HTTP client cannot be built.
    pub fn new(api_base: &str, repo: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Thirty seconds is not a download budget; it is the budget for
            // the JSON. A slow DNS or a hung TLS handshake has to end in an
            // error the user can read, not in a command that appears frozen
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!(
                "cormex/",
                env!("CARGO_PKG_VERSION"),
                " (self-update)"
            ))
            .build()
            .context("could not build the HTTP client")?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            repo: repo.trim_matches('/').to_owned(),
        })
    }

    /// The newest published release.
    ///
    /// # Errors
    /// The API is unreachable, rate-limited, has no releases, or answers with
    /// something that is not a release.
    pub async fn latest(&self) -> Result<Release> {
        self.release_json(&format!("/repos/{}/releases/latest", self.repo))
            .await
    }

    /// One release by tag.
    ///
    /// # Errors
    /// As [`Self::latest`], plus "no such tag".
    pub async fn by_tag(&self, tag: &str) -> Result<Release> {
        self.release_json(&format!("/repos/{}/releases/tags/{tag}", self.repo))
            .await
    }

    async fn release_json(&self, path: &str) -> Result<Release> {
        let url = format!("{}{path}", self.api_base);
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .with_context(|| format!("could not reach {url}"))?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "{url} returned 404: either no release with that tag exists, or the repository \
                 has not published one yet (https://github.com/{}/releases)",
                self.repo
            );
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            anyhow::bail!(rate_limited(&response));
        }
        if !status.is_success() {
            anyhow::bail!("{url} returned {status}");
        }
        response
            .json::<Release>()
            .await
            .with_context(|| format!("the release at {url} did not parse"))
    }

    /// Fetch one asset.
    ///
    /// Held in memory rather than streamed to a file: the archives are a few
    /// megabytes, the checksum has to be computed over the whole thing before
    /// anything is written anywhere, and a partial file on disk is a failure
    /// mode that has to be cleaned up.
    ///
    /// # Errors
    /// The download fails, or the server answers with a non-success status.
    pub async fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        let url = &asset.browser_download_url;
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .with_context(|| format!("could not download {url}"))?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("{url} returned {status}");
        }
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("{url} broke off mid-download"))?;
        Ok(bytes.to_vec())
    }

    /// The release's `SHA256SUMS`, as text.
    ///
    /// # Errors
    /// The release has no `SHA256SUMS`, or the download fails.
    pub async fn sums(&self, release: &Release) -> Result<String> {
        let asset = release.asset("SHA256SUMS").ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no SHA256SUMS, so there is no way to tell a corrupt download \
                 from a tampered one. Assets: {}",
                release.tag_name,
                release.asset_names()
            )
        })?;
        let bytes = self.download(asset).await?;
        String::from_utf8(bytes).context("SHA256SUMS is not UTF-8; it is not a checksums file")
    }
}

/// The message for an exhausted rate limit.
///
/// Worth its own sentence because the 403 is otherwise indistinguishable from
/// "the repository is private", and the fix (wait, or authenticate) is entirely
/// different.
fn rate_limited(response: &reqwest::Response) -> String {
    let remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if remaining != "0" {
        return format!(
            "GitHub refused the request ({}). If the repository has moved or been made private, \
             this binary cannot update itself",
            response.status()
        );
    }
    let reset = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|at| at.to_rfc3339());
    match reset {
        Some(at) => format!(
            "GitHub's unauthenticated API rate limit is exhausted; it resets at {at}. \
             `cormex update` uses no token, so this is per-IP and shared with anything else \
             on this machine that talks to the API"
        ),
        None => "GitHub's unauthenticated API rate limit is exhausted. Try again later.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> Release {
        serde_json::from_value(serde_json::json!({
            "tag_name": "v0.1.14",
            "assets": [
                {"name": "SHA256SUMS", "size": 600, "browser_download_url": "http://x/SHA256SUMS"},
                {"name": "cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz", "size": 4_200_000,
                 "browser_download_url": "http://x/a.tar.gz"}
            ]
        }))
        .expect("the shape GitHub documents")
    }

    #[test]
    fn versions_compare_numerically_not_as_text() {
        // The classic bug: "0.1.9" > "0.1.10" as strings, so the updater would
        // keep offering 0.1.10 to someone already on it
        assert!(Version::parse("0.1.10").unwrap() > Version::parse("0.1.9").unwrap());
        assert!(Version::parse("0.2.0").unwrap() > Version::parse("0.1.99").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
        assert_eq!(
            Version::parse("0.1.13").unwrap(),
            Version::parse("v0.1.13").unwrap(),
            "the tag carries a `v`, the version does not"
        );
    }

    #[test]
    fn a_version_is_exactly_three_numbers() {
        assert_eq!(
            Version::parse("v0.1.13").unwrap(),
            Version {
                major: 0,
                minor: 1,
                patch: 13
            }
        );
        for bad in [
            "",
            "v",
            "0.1",
            "0.1.13.1",
            "0.1.13-rc1",
            "latest",
            "0.1.x",
            "v0.1.13 ",
        ] {
            assert!(
                Version::parse(bad).is_none(),
                "{bad:?} is not a release version, and accepting it would compare as a number"
            );
        }
    }

    #[test]
    fn a_version_prints_the_way_the_tag_reads_without_the_v() {
        assert_eq!(Version::parse("v0.1.14").unwrap().to_string(), "0.1.14");
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_asset_is_found_by_its_exact_name() {
        let r = release();
        assert!(r.asset("SHA256SUMS").is_some());
        assert_eq!(r.version(), Version::parse("0.1.14"));
        assert!(
            r.asset("cormex-0.1.14-x86_64-unknown-linux-musl.tar.gz")
                .is_some()
        );
        // A prefix must not match: the six assets differ only in their tail
        assert!(r.asset("cormex-0.1.14").is_none());
        assert!(r.asset_names().contains("SHA256SUMS"));
    }
}
