//! The sync engine: discovery -> decision -> upload.
//!
//! Design: docs/design/session-archive-architecture.md §5.3.
//!
//! # One pass, no daemon
//!
//! `run` does one pass over the discovered files and returns. That is the whole
//! design (D1): the client is invoked by a hook or a scheduler, does its work,
//! and exits. A long-lived watcher would need a reconnect strategy, a way to
//! survive suspend, and a story for what happens when two of them run — all of
//! which the "invoke, work, exit" shape gets for free.
//!
//! # What one file's pass does
//!
//! 1. Take the file's lock; skip if another process has it.
//! 2. Read the local state and observe the file (size, tail hash, whole hash).
//! 3. `plan::decide` — pure, and every branch is a rule from the design.
//! 4. Act: register, open a generation, or upload from an offset.
//! 5. Write the state back **in the same step as each chunk's ack**, so a crash
//!    never leaves "uploaded" recorded for bytes the server does not have.

pub mod api;
mod classify;
pub mod fake;
pub mod lock;
pub mod plan;
pub mod reader;
pub mod reconcile;
pub mod upload;

use anyhow::Result;
use cormex_proto::capture::{
    FileClassification, FileEventOp, FileEventRequest, RegisterFileRequest,
};

use crate::catalog::{DiscoveredFile, discover_with_warnings};
use crate::config::Config;
use crate::state::{FileState, StateDb};
use crate::sync::api::CaptureApi;
use crate::sync::plan::{Action, NewGenerationReason, Observed};
use crate::sync::reader::LogicalReader;
use crate::sync::upload::Budget;

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub files_seen: usize,
    pub files_uploaded: usize,
    pub bytes: i64,
    pub errors: usize,
    /// Files left for the next run because the budget ran out.
    pub deferred: usize,
    /// Files skipped because another process held their lock.
    pub locked: usize,
    /// The first error, for `doctor`.
    pub last_error: Option<String>,
}

/// How a run was triggered, for the `runs` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Hook,
    Scheduled,
    Manual,
    Queue,
}

impl Trigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::Queue => "queue",
        }
    }
}

/// What a run should do.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub trigger: Trigger,
    /// Only this logical path, when a hook named one.
    pub only: Option<String>,
    pub budget: Budget,
    /// Decide and report, upload nothing.
    pub dry_run: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            trigger: Trigger::Manual,
            only: None,
            budget: Budget::default_budget(),
            dry_run: false,
        }
    }
}

/// Run one pass.
///
/// # Errors
/// A catalog glob does not compile, or the state database cannot be opened.
/// Per-file failures are **counted, not returned**: one unreadable file must
/// not stop the other fifty.
pub async fn run(
    api: &dyn CaptureApi,
    db: &StateDb,
    data_dir: &std::path::Path,
    catalog: &cormex_proto::catalog::Catalog,
    cfg: &Config,
    device_id: &str,
    opts: &SyncOptions,
) -> Result<SyncReport> {
    let started = chrono::Utc::now();
    let mut report = SyncReport::default();

    // **One budget for the whole run**, shared across files. A per-file budget
    // would let a run with a hundred small files take a hundred times as long
    // as the design allows.
    let mut budget = opts.budget.clone();

    // Take a snapshot first, so events arriving during the scan are retained.
    let queued = if !opts.dry_run && opts.only.is_none() {
        db.queue()?
    } else {
        Vec::new()
    };
    let spool = if !opts.dry_run && opts.only.is_none() {
        std::fs::read_dir(crate::hook::spool_dir(data_dir))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_type().is_ok_and(|t| t.is_file())
                    && e.path().extension().is_some_and(|ext| ext == "json")
            })
            .filter_map(|e| {
                let path = e.path();
                let content: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
                Some((
                    path,
                    std::path::PathBuf::from(content.get("path")?.as_str()?),
                    content
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                ))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut cwd_hints = std::collections::HashMap::new();
    for entry in &queued {
        if let Some(cwd) = entry.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            cwd_hints.insert(
                path_identity(
                    std::path::Path::new(&entry.path),
                    Some(std::path::Path::new(cwd)),
                ),
                cwd.to_owned(),
            );
        }
    }
    for (_, source, cwd) in &spool {
        if let Some(cwd) = cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            cwd_hints.insert(
                path_identity(source, Some(std::path::Path::new(cwd))),
                cwd.to_owned(),
            );
        }
    }

    let (mut files, warnings) = discover_with_warnings(catalog, cfg, device_id)?;
    for warning in warnings {
        report.errors += 1;
        if report.last_error.is_none() {
            report.last_error = Some(warning.clone());
        }
        tracing::warn!("source discovery failed: {warning}");
    }
    if let Some(only) = &opts.only {
        files.retain(|f| &f.logical_path == only);
    }
    report.files_seen = files.len();
    let mut handled_paths = std::collections::HashSet::new();
    let mut project_cache = classify::ProjectCache::new();

    for file in &mut files {
        if budget.exhausted() {
            report.deferred += 1;
            continue;
        }

        if let Some(cwd) = cwd_hints.get(&path_identity(&file.physical_path, None)) {
            file.cwd = Some(cwd.clone());
            file.cwd_guessed = false;
        }
        let classification = classify::for_file(file, cfg, &mut project_cache);
        let local_key = format!("{}|{}", file.agent, file.logical_path);
        let Some(_lock) = lock::FileLock::try_acquire(data_dir, &local_key)? else {
            report.locked += 1;
            continue;
        };

        match sync_one(
            api,
            db,
            file,
            classification.as_ref(),
            &local_key,
            opts,
            &mut budget,
        )
        .await
        {
            Ok(Some(uploaded)) => {
                if uploaded > 0 {
                    report.files_uploaded += 1;
                    report.bytes += uploaded;
                }
                if !budget.exhausted() {
                    handled_paths.insert(file.physical_path.clone());
                }
            }
            Ok(None) => {
                if !budget.exhausted() {
                    handled_paths.insert(file.physical_path.clone());
                }
            }
            Err(e) => {
                report.errors += 1;
                if report.last_error.is_none() {
                    report.last_error = Some(format!("{}: {e}", file.logical_path));
                }
                tracing::warn!(
                    agent = %file.agent,
                    path = %file.logical_path,
                    error = %e,
                    "could not sync this file; the rest of the run continues"
                );
            }
        }
    }

    if !opts.dry_run
        && report.errors == 0
        && report.deferred == 0
        && report.locked == 0
        && !budget.exhausted()
    {
        for entry in queued {
            if handled_paths.contains(std::path::Path::new(&entry.path)) {
                db.dequeue(entry.id)?;
            }
        }
        for (path, source, _) in spool {
            if handled_paths.contains(&source)
                && let Err(error) = std::fs::remove_file(&path)
            {
                tracing::warn!(?path, %error, "could not remove processed spool file");
            }
        }
    }

    // A run with zero errors is the only one `doctor` counts as a success, so
    // a dry run must not record one: it uploaded nothing, and recording it
    // would make "last successful sync" a lie
    if !opts.dry_run {
        db.record_run(
            opts.trigger.as_str(),
            &started.to_rfc3339(),
            &chrono::Utc::now().to_rfc3339(),
            i64::try_from(report.files_uploaded).unwrap_or(0),
            report.bytes,
            i64::try_from(report.errors).unwrap_or(0),
            report.last_error.as_deref(),
        )?;
    }

    Ok(report)
}

/// One file. `Ok(None)` = nothing to do.
async fn sync_one(
    api: &dyn CaptureApi,
    db: &StateDb,
    file: &DiscoveredFile,
    classification: Option<&FileClassification>,
    local_key: &str,
    opts: &SyncOptions,
    budget: &mut Budget,
) -> Result<Option<i64>> {
    // A SQLite row has no file to open: its bytes were taken at discovery
    // time, and re-reading would race the host's own writer
    let reader = match &file.bytes {
        Some(bytes) => LogicalReader::from_bytes(bytes.clone()),
        None => LogicalReader::open(&file.physical_path, file.compressed)?,
    };
    let snapshot = file.framing == cormex_proto::capture::Framing::Snapshot;

    let mut state = db.file(local_key)?.unwrap_or_else(|| FileState {
        local_key: local_key.to_owned(),
        agent: file.agent.clone(),
        path: file.physical_path.display().to_string(),
        file_id: None,
        generation_id: None,
        generation_ordinal: None,
        uploaded_offset: 0,
        tail_sha256: None,
        whole_sha256: None,
        last_size: None,
        last_mtime_ns: None,
        missing_reported: false,
        last_ok_at: None,
        classification_cwd: None,
        classification_project_key: None,
        classification_sent: false,
    });

    let observed = Observed {
        size: i64::try_from(reader.len()).unwrap_or(i64::MAX),
        tail_sha256: reader.tail_hash(u64::try_from(state.uploaded_offset).unwrap_or(0))?,
        whole_sha256: if snapshot {
            Some(reader.whole_hash()?)
        } else {
            None
        },
        snapshot,
    };

    let action = plan::decide(
        if state.file_id.is_some() {
            Some(&state)
        } else {
            None
        },
        &observed,
    );

    if opts.dry_run {
        tracing::info!(path = %file.logical_path, ?action, "dry run: would act");
        return Ok(None);
    }

    let mut classification_changed = false;
    if let (Some(file_id), Some(classification)) = (state.file_id.as_deref(), classification)
        && (!state.classification_sent
            || state.classification_cwd.as_deref() != classification.cwd.as_deref()
            || state.classification_project_key.as_deref() != classification.project_key.as_deref())
    {
        api.update_classification(file_id, classification).await?;
        state.classification_cwd = classification.cwd.clone();
        state.classification_project_key = classification.project_key.clone();
        state.classification_sent = true;
        classification_changed = true;
    }

    match action {
        Action::Skip => {
            if classification_changed {
                db.put_file(&state)?;
            }
            Ok(None)
        }
        Action::Register => {
            let resp = api
                .register(&RegisterFileRequest {
                    agent: file.agent.clone(),
                    native_session_id: file.native_session_id.clone(),
                    parent_native_session_id: file.parent_native_session_id.clone(),
                    logical_path: file.logical_path.clone(),
                    role: file.role,
                    framing: file.framing,
                    cwd: classification
                        .and_then(|classification| classification.cwd.clone())
                        .or_else(|| file.cwd.clone()),
                    project_key: classification
                        .and_then(|classification| classification.project_key.clone()),
                })
                .await?;
            let file_id = resp.file_id.clone();
            state.file_id = Some(resp.file_id);

            // ── Never resume on an offset alone ──
            //
            // Reaching `Register` means this machine has no memory of the file:
            // first sight, `state.db` lost, or a reinstall. The server may
            // already hold this exact stream — under this file (state lost),
            // or under another file of the same session (a new device key, a
            // host-side move, a second machine). Each case is settled by a
            // prefix hash, not by trusting where the server says it ends.
            let local = observed.size;
            let mut generation = resp.generation.clone();
            let mut from = 0_i64;
            if generation.end_offset > 0 {
                match reconcile::compare(api, &reader, &generation.id, generation.end_offset)
                    .await?
                {
                    reconcile::Verdict::Extends { end } => from = end,
                    // An older copy of what the server already has: nothing new
                    reconcile::Verdict::Behind => from = local,
                    reconcile::Verdict::Diverged => {
                        // Rewritten while this machine was not watching. A new
                        // generation, not a splice onto the old history
                        generation = api
                            .open_generation(
                                &file_id,
                                if snapshot { "snapshot" } else { "rewrite" },
                            )
                            .await?;
                    }
                }
            } else if !snapshot {
                // A file the server has never seen under this name. If its bytes
                // are a stream the session already has, follow that stream
                for candidate in &resp.candidates {
                    let verdict =
                        reconcile::compare(api, &reader, &candidate.id, candidate.end_offset)
                            .await?;
                    let next_from = match verdict {
                        reconcile::Verdict::Extends { end } => end,
                        reconcile::Verdict::Behind => local,
                        reconcile::Verdict::Diverged => continue,
                    };
                    generation = api
                        .adopt(&file_id, &candidate.id, next_from.min(candidate.end_offset))
                        .await?;
                    from = next_from;
                    tracing::info!(
                        path = %file.logical_path,
                        generation = %generation.id,
                        "already archived under another name; following that copy instead of \
                         uploading it again"
                    );
                    break;
                }
            }

            state.generation_id = Some(generation.id.clone());
            state.generation_ordinal = Some(generation.ordinal);
            state.uploaded_offset = from;
            state.tail_sha256 = reader.tail_hash(u64::try_from(from).unwrap_or(0))?;
            state.whole_sha256 = observed.whole_sha256.clone();
            state.last_size = Some(observed.size);
            if let Some(classification) = classification {
                state.classification_cwd = classification.cwd.clone();
                state.classification_project_key = classification.project_key.clone();
                state.classification_sent = true;
            }
            db.put_file(&state)?;

            let uploaded =
                upload::upload(api, db, &reader, &mut state, &generation.id, from, budget).await?;
            Ok(Some(uploaded.bytes))
        }
        Action::Append { from } => {
            let generation = state
                .generation_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("the state has no generation to append to"))?;
            let uploaded =
                upload::upload(api, db, &reader, &mut state, &generation, from, budget).await?;
            state.last_size = Some(observed.size);
            state.whole_sha256 = observed.whole_sha256.clone();
            db.put_file(&state)?;
            Ok(Some(uploaded.bytes))
        }
        Action::NewGeneration { reason } => {
            let file_id = state
                .file_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("the state has no file id"))?;
            let generation = api.open_generation(&file_id, reason_str(reason)).await?;
            state.generation_id = Some(generation.id.clone());
            state.generation_ordinal = Some(generation.ordinal);
            state.uploaded_offset = 0;
            state.tail_sha256 = None;
            state.whole_sha256 = observed.whole_sha256.clone();
            state.last_size = Some(observed.size);
            db.put_file(&state)?;

            let uploaded =
                upload::upload(api, db, &reader, &mut state, &generation.id, 0, budget).await?;
            Ok(Some(uploaded.bytes))
        }
        Action::Snapshot => {
            // `decide` only returns this for a snapshot source whose state has
            // no generation yet; registration handles the first one
            if classification_changed {
                db.put_file(&state)?;
            }
            Ok(None)
        }
    }
}

/// One comparison key for a hook path and a discovered path.
///
/// Hooks normally send absolute paths, but resolving first also handles a
/// relative transcript path and Windows case differences. If the file vanished
/// between discovery and this check, the lexical path is still stable enough.
fn path_identity(path: &std::path::Path, base: Option<&std::path::Path>) -> String {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base) = base {
        base.join(path)
    } else {
        path.to_path_buf()
    };
    let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
    let value = resolved.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn reason_str(r: NewGenerationReason) -> &'static str {
    match r {
        NewGenerationReason::Rewrite => "rewrite",
        NewGenerationReason::Truncate => "truncate",
        NewGenerationReason::Snapshot => "snapshot",
    }
}

/// Report files that have disappeared, **once each**.
///
/// # Errors
/// The state database cannot be read.
pub async fn report_missing(
    api: &dyn CaptureApi,
    db: &StateDb,
    files: &[DiscoveredFile],
) -> Result<usize> {
    let present: std::collections::HashSet<&str> =
        files.iter().map(|f| f.logical_path.as_str()).collect();
    let mut reported = 0;
    for file in files {
        if present.contains(file.logical_path.as_str()) {
            continue;
        }
        let local_key = format!("{}|{}", file.agent, file.logical_path);
        let Some(mut state) = db.file(&local_key)? else {
            continue;
        };
        if state.missing_reported {
            continue;
        }
        if let Some(file_id) = state.file_id.clone() {
            api.file_event(
                &file_id,
                &FileEventRequest {
                    op: FileEventOp::Missing,
                    detail: None,
                },
            )
            .await?;
            state.missing_reported = true;
            db.put_file(&state)?;
            reported += 1;
        }
    }
    Ok(reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trigger_spellings_are_stable() {
        // These land in `runs.trigger`, which `doctor` reads
        assert_eq!(Trigger::Hook.as_str(), "hook");
        assert_eq!(Trigger::Scheduled.as_str(), "scheduled");
        assert_eq!(Trigger::Manual.as_str(), "manual");
        assert_eq!(Trigger::Queue.as_str(), "queue");
    }

    #[test]
    fn the_generation_reasons_use_the_wire_spellings() {
        assert_eq!(reason_str(NewGenerationReason::Rewrite), "rewrite");
        assert_eq!(reason_str(NewGenerationReason::Truncate), "truncate");
        assert_eq!(reason_str(NewGenerationReason::Snapshot), "snapshot");
    }
}
