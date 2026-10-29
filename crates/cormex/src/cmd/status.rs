//! `cormex status` — is this machine set up, and is it keeping up?
//!
//! # Why it reports the credential's storage location
//!
//! "Where is my device key" is a question with three answers (environment,
//! keyring, 0600 file), and the third one is a fact the user should know about
//! rather than discover. A key in a file on a shared machine is a real risk,
//! and it is only a risk while nobody knows.

use anyhow::Result;

use crate::client::ApiClient;
use crate::config::Config;
use crate::credentials;
use crate::state::StateDb;

/// What `status` found.
#[derive(Debug, Clone)]
pub struct Status {
    pub server_url: String,
    pub reachable: bool,
    pub authenticated: bool,
    /// Who the server says we are, when it answered.
    pub username: Option<String>,
    pub device_id: Option<String>,
    pub credential_source: &'static str,
    /// Queue entries waiting to be sent.
    pub queued: i64,
    /// When the last run without errors finished.
    pub last_ok_at: Option<String>,
}

/// Collect the status. Never fails on "the server is down": that is one of the
/// things it reports.
///
/// # Errors
/// The local state database cannot be read.
pub async fn collect(
    cfg: &Config,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<Status> {
    let db = StateDb::open(data_dir)?;
    let queued = db.queue_len()?;
    let last_ok_at = db.last_successful_run()?;

    let mut status = Status {
        server_url: cfg.server_url.clone(),
        reachable: false,
        authenticated: false,
        username: None,
        device_id: None,
        credential_source: "none",
        queued,
        last_ok_at,
    };

    let Ok((key, source)) = credentials::load(config_dir, &cfg.server_url) else {
        return Ok(status);
    };
    status.credential_source = source.as_str();

    let Ok(client) = ApiClient::new(&cfg.server_url, &key) else {
        return Ok(status);
    };
    match client
        .get_json::<cormex_proto::auth::WhoAmI>("/auth/me")
        .await
    {
        Ok(me) => {
            status.reachable = true;
            status.authenticated = true;
            status.username = Some(me.username);
            status.device_id = me.device_id;
        }
        Err(e) if e.code == crate::client::ExitCode::Unreachable => {
            status.reachable = false;
        }
        Err(_) => {
            // Reachable but refused: the server answered, the key is wrong
            status.reachable = true;
            status.authenticated = false;
        }
    }
    Ok(status)
}

/// Render for a terminal.
#[must_use]
pub fn render(s: &Status) -> String {
    let mut out = String::new();
    out.push_str(&format!("server      {}\n", s.server_url));
    out.push_str(&format!(
        "reachable   {}\n",
        if s.reachable { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "signed in   {}\n",
        match (&s.authenticated, &s.username) {
            (true, Some(u)) => format!("yes ({u})"),
            (true, None) => "yes".to_owned(),
            (false, _) => "no".to_owned(),
        }
    ));
    if let Some(id) = &s.device_id {
        out.push_str(&format!("device      {id}\n"));
    }
    out.push_str(&format!("credential  {}\n", s.credential_source));
    out.push_str(&format!("queued      {}\n", s.queued));
    out.push_str(&format!(
        "last ok     {}\n",
        s.last_ok_at.as_deref().unwrap_or("never")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendering says "never" rather than leaving the line blank.
    ///
    /// A blank line reads as "not applicable"; "never" is the fact, and it is
    /// the one that tells the user something is wrong.
    #[test]
    fn a_never_run_is_stated_not_blank() {
        let s = Status {
            server_url: "https://m".into(),
            reachable: false,
            authenticated: false,
            username: None,
            device_id: None,
            credential_source: "none",
            queued: 3,
            last_ok_at: None,
        };
        let text = render(&s);
        assert!(text.contains("last ok     never"), "{text}");
        assert!(text.contains("queued      3"), "{text}");
    }
}
