//! Host metadata — the title, project and pin a desktop app shows for a
//! session — sent on every sync, only when it changed.
//!
//! # Why every sync, and why only changes
//!
//! This used to happen only on `cormex sync --codex-metadata`, which nothing
//! ran: hooks and the scheduler call plain `cormex sync`. So a session renamed,
//! moved into a project or pinned in the Codex desktop app kept its old place
//! in the archive until someone remembered the flag.
//!
//! Reading the metadata is cheap (one SQLite query and one JSON file per Codex
//! home); sending it for hundreds of sessions every few minutes is not. The
//! hash of what was last sent for each file sits in `state.db`, and a session
//! is sent again only when its hash differs — the first sync after upgrading
//! sends everything once, and after that only what the user changed.
//!
//! # Not fatal
//!
//! Codex may hold its database locked, or a send may fail. Either is logged
//! and retried next run (the hash is stored only after a successful send); it
//! does not turn the sync red, because the transcripts — the thing a sync
//! exists for — were captured.

use std::collections::HashMap;

use anyhow::Result;
use cormex_proto::capture::{FileRole, SessionUiMetadata};
use cormex_proto::catalog::Catalog;

use crate::catalog::{DiscoveredFile, resolve_homes};
use crate::config::Config;
use crate::state::StateDb;
use crate::sync::api::CaptureApi;

/// What the metadata pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataOutcome {
    pub updated: usize,
    pub failed: usize,
}

/// Send the Codex desktop metadata for the discovered Codex transcripts whose
/// metadata changed since it was last sent — or all of it, with `force`.
///
/// # Errors
/// `state.db` cannot be read or written. Reading Codex's own files and sending
/// to the server are not errors; they are counted in `failed`.
pub async fn sync_codex(
    api: &dyn CaptureApi,
    db: &StateDb,
    catalog: &Catalog,
    cfg: &Config,
    files: &[DiscoveredFile],
    force: bool,
) -> Result<MetadataOutcome> {
    let mut outcome = MetadataOutcome::default();
    let Some(entry) = catalog.agents.iter().find(|a| a.id == "codex") else {
        return Ok(outcome);
    };
    // Every Codex home: a CLI with its own CODEX_HOME and the desktop app on the
    // default both keep a sidebar of their own
    let mut metadata: HashMap<String, SessionUiMetadata> = HashMap::new();
    for home in resolve_homes(entry, cfg) {
        match crate::codex_metadata::read(&home.path) {
            Ok(found) => {
                for (session, item) in found {
                    metadata.entry(session).or_insert(item);
                }
            }
            Err(error) => {
                outcome.failed += 1;
                tracing::warn!(
                    home = %home.path.display(),
                    %error,
                    "could not read the Codex desktop metadata; trying again next sync"
                );
            }
        }
    }
    if metadata.is_empty() {
        return Ok(outcome);
    }

    for file in files
        .iter()
        .filter(|f| f.agent == "codex" && f.role == FileRole::Transcript)
    {
        let Some(item) = metadata.get(&file.native_session_id) else {
            continue;
        };
        let local_key = format!("{}|{}", file.agent, file.logical_path);
        // A file the transcript pass has not registered yet has nowhere to put
        // metadata; it gets it on the run that registers it
        let Some(file_id) = db.file(&local_key)?.and_then(|s| s.file_id) else {
            continue;
        };
        let sha = crate::sync::reader::sha256_hex(&serde_json::to_vec(item)?);
        if !force && db.metadata_sent(&local_key)?.as_deref() == Some(sha.as_str()) {
            continue;
        }
        match api.update_metadata(&file_id, item).await {
            Ok(()) => {
                db.put_metadata_sent(&local_key, &sha)?;
                outcome.updated += 1;
            }
            Err(error) => {
                outcome.failed += 1;
                tracing::warn!(
                    path = %file.logical_path,
                    %error,
                    "could not send the Codex metadata; trying again next sync"
                );
            }
        }
    }
    Ok(outcome)
}
