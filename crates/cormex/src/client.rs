//! The HTTP client: a thin wrapper that maps failures onto exit codes.
//!
//! # Why the exit codes are part of the interface
//!
//! `cormex sync` is run by a scheduler and by hooks. The scheduler decides
//! whether to alert a human, and it can only do that by looking at the exit
//! code — so "the server is unreachable, everything is queued" and "the server
//! rejected our credential" have to be **different numbers**. Collapsing both
//! into 1 means an expired key pages somebody at 3am with "sync failed", or a
//! network blip gets treated as a permanent error and the queue is dropped.

use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;

/// The process exit codes. Design §5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Everything worked.
    Ok = 0,
    /// The command line was wrong.
    Usage = 2,
    /// Not logged in, or the credential was rejected.
    Unauthenticated = 3,
    /// The server could not be reached; work is queued for next time.
    Unreachable = 4,
    /// Some sources failed; the rest were captured.
    PartialFailure = 5,
}

impl ExitCode {
    /// The number `main` returns.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// A failure with the exit code it maps to.
#[derive(Debug)]
pub struct ClientError {
    pub code: ExitCode,
    pub message: String,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

/// The API client.
#[derive(Clone)]
pub struct ApiClient {
    base: String,
    http: reqwest::Client,
    key: String,
}

impl ApiClient {
    /// Build a client for one server with one device key.
    ///
    /// The timeouts are deliberately short: this runs from a hook whose host
    /// gives it a second or two, so a request that would take longer has
    /// already failed as far as the caller is concerned. The connect timeout is
    /// shorter still — an unreachable server should be discovered in five
    /// seconds, not thirty.
    ///
    /// # Errors
    /// The base URL is not a URL, or the HTTP client cannot be built.
    pub fn new(base: &str, key: &str) -> Result<Self> {
        let base = base.trim().trim_end_matches('/').to_owned();
        if base.is_empty() {
            anyhow::bail!("the server URL is empty; run `cormex login --server <URL>`");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("cormex/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the HTTP client")?;
        Ok(Self {
            base,
            http,
            key: key.to_owned(),
        })
    }

    /// The server this client talks to.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET`, deserialised.
    ///
    /// # Errors
    /// [`ExitCode::Unreachable`] when the request never completed,
    /// [`ExitCode::Unauthenticated`] on 401/403, and a plain error otherwise.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(unreachable)?;
        self.decode(resp).await
    }

    /// `POST` JSON, deserialised.
    ///
    /// # Errors
    /// As [`Self::get_json`].
    pub async fn post_json<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .map_err(unreachable)?;
        self.decode(resp).await
    }

    /// `POST` with no body, expecting no body back.
    ///
    /// # Errors
    /// As [`Self::get_json`].
    pub async fn post_empty(&self, path: &str) -> Result<(), ClientError> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(unreachable)?;
        self.check_status(resp.status(), path)?;
        Ok(())
    }

    /// `PUT` raw bytes with extra headers, returning the raw response.
    ///
    /// The capture chunk endpoint takes a byte body and carries its metadata in
    /// headers, so neither the JSON helper nor a plain `post` can express it.
    ///
    /// # Errors
    /// As [`Self::get_json`], plus the status and body of a 4xx so the caller
    /// can distinguish 409 (resume from `expected_offset`) from 422 (the bytes
    /// are wrong).
    pub async fn put_bytes(
        &self,
        path: &str,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), ClientError> {
        let mut req = self
            .http
            .put(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .body(body);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let resp = req.send().await.map_err(unreachable)?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(unreachable)?.to_vec();
        Ok((status, bytes))
    }

    /// `DELETE`, expecting no body.
    ///
    /// # Errors
    /// As [`Self::get_json`].
    pub async fn delete(&self, path: &str) -> Result<(), ClientError> {
        let resp = self
            .http
            .delete(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(unreachable)?;
        self.check_status(resp.status(), path)?;
        Ok(())
    }

    async fn decode<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T, ClientError> {
        let status = resp.status();
        let path = resp.url().path().to_owned();
        self.check_status(status, &path)?;
        resp.json::<T>().await.map_err(|e| ClientError {
            code: ExitCode::PartialFailure,
            message: format!("the server's response to {path} does not parse: {e}"),
        })
    }

    fn check_status(&self, status: reqwest::StatusCode, path: &str) -> Result<(), ClientError> {
        if status.is_success() {
            return Ok(());
        }
        // 401 and 403 are the credential's fault and no amount of retrying
        // fixes them; everything else is either the server's problem or ours,
        // and the caller decides
        let code = if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            ExitCode::Unauthenticated
        } else {
            ExitCode::PartialFailure
        };
        Err(ClientError {
            code,
            message: format!("{path} returned {status}"),
        })
    }
}

fn unreachable(e: reqwest::Error) -> ClientError {
    ClientError {
        code: ExitCode::Unreachable,
        message: format!("could not reach the server: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_server_url_is_refused() {
        let err = match ApiClient::new("   ", "key") {
            Ok(_) => panic!("an empty URL is not usable"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("cormex login"),
            "the error should name the command that sets it: {err}"
        );
    }

    #[test]
    fn the_exit_codes_are_the_documented_numbers() {
        // These are in the design and in `cormex --help`; changing one silently
        // changes what every scheduler interprets
        assert_eq!(ExitCode::Ok.code(), 0);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::Unauthenticated.code(), 3);
        assert_eq!(ExitCode::Unreachable.code(), 4);
        assert_eq!(ExitCode::PartialFailure.code(), 5);
    }

    #[test]
    fn a_trailing_slash_is_normalised() {
        let c = ApiClient::new("https://memory.example/", "k").expect("build");
        assert_eq!(
            c.base(),
            "https://memory.example",
            "a trailing slash would make every path `//capture/...`, which some proxies 404"
        );
    }
}
