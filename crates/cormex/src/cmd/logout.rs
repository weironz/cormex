//! `cormex logout` — revoke this machine's key and forget it.
//!
//! # Order matters: revoke first, then forget
//!
//! Clearing the local copy first would leave a **live** key on the server that
//! nobody can find any more. It would keep working until somebody noticed it in
//! the device list, and the user's mental model ("I logged out") would be
//! wrong.
//!
//! A revocation that fails because the server is unreachable is reported, and
//! the local copy is **kept**: deleting it would leave the key live *and*
//! unreachable, which is the worst of both.

use anyhow::Result;

use crate::client::ApiClient;
use crate::config::Config;
use crate::credentials;

/// Revoke and clear.
///
/// # Errors
/// There is no stored key, or the server refused the revocation.
pub async fn run(cfg: &Config, config_dir: &std::path::Path) -> Result<()> {
    let (key, _source) = credentials::load(config_dir, &cfg.server_url)?;

    // "Who am I" tells us which device id to revoke. Without it we would have
    // to revoke every device on the account, which is not what logout means
    let client = ApiClient::new(&cfg.server_url, &key)?;
    let me: cormex_proto::auth::WhoAmI = client
        .get_json("/auth/me")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let _ = me;

    // The device id is not in `/auth/me`; the device list is the way to find
    // the one whose key we hold. A key can only see itself there, so the list
    // is exactly the one entry.
    let devices: cormex_proto::devices::DevicesResponse = client
        .get_json("/devices")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(device) = devices.devices.into_iter().next() else {
        anyhow::bail!(
            "the server does not recognise this key, so there is nothing to revoke. Run \
             `cormex logout --forget` to clear the local copy anyway"
        );
    };

    client
        .delete(&format!("/devices/{}", device.id))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    credentials::clear(config_dir, &cfg.server_url)?;
    println!("revoked device {} ({})", device.id, device.label);
    Ok(())
}

/// Clear the local copy without contacting the server.
///
/// # Errors
/// The credential store could not be cleared.
pub fn forget(cfg: &Config, config_dir: &std::path::Path) -> Result<()> {
    credentials::clear(config_dir, &cfg.server_url)?;
    println!("the local device key is gone");
    println!(
        "⚠ the key may still be valid on the server -- revoke it in the Web UI's device list \
         if this machine is being retired"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `forget` says out loud that the server copy may still be live.
    ///
    /// The failure this guards is a user retiring a laptop with `--forget`,
    /// believing it is done, while the key keeps working.
    #[test]
    fn forget_warns_that_the_server_copy_may_still_be_live() {
        let src = include_str!("logout.rs");
        let body = src
            .split("pub fn forget")
            .nth(1)
            .expect("forget is still here");
        assert!(
            body.contains("may still be valid"),
            "forget no longer warns that the server-side key survives -- a retired laptop would \
             keep uploading"
        );
    }
}
