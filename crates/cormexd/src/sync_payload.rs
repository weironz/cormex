//! Outward serialization of sync payloads.
//!
//! `cormex_store::SyncPayload` deliberately does not derive `Serialize`; the
//! mapping is written out explicitly here, for two reasons:
//!
//! 1. **`tsv` and `embedding` are server-derived columns and never enter a sync
//!    payload** (a hard rule from memory.md §8). They are bulky, useless to the
//!    client, and they change wholesale whenever the embedding model changes,
//!    blowing up incremental sync for nothing.
//! 2. The storage layer's structs are an internal representation. Letting them
//!    dictate the wire protocol directly turns every internal refactor into a
//!    protocol break.
//!
//! The price is that a new field needs one extra line here. That is **deliberate
//! friction** — it forces someone to decide whether the client should be given
//! this field at all.

use cormex_store::SyncPayload;
use serde_json::{Value, json};

/// Convert a storage-layer payload into the JSON sent down to clients.
pub fn to_json(p: &SyncPayload) -> Value {
    match p {
        SyncPayload::Episode(e) => json!({
            "id": e.id,
            "session_id": e.session_id,
            "role": e.role.as_str(),
            "content": e.content,
            "text": e.text,
            "domain": e.domain,
            "device_id": e.device_id,
            "occurred_at": e.occurred_at.to_rfc3339(),
            "created_at": e.created_at.to_rfc3339(),
        }),

        SyncPayload::Blob(b) => json!({
            "hash": b.hash,
            "mime": b.mime,
            "size_bytes": b.size_bytes,
            // storage_key is not sent down: clients should fetch objects through
            // a presigned URL, not connect directly using an internal storage
            // path
            "created_at": b.created_at.to_rfc3339(),
        }),

        SyncPayload::EpisodeBlob(eb) => json!({
            "episode_id": eb.episode_id,
            "blob_hash": eb.blob_hash,
            "kind": eb.kind,
            // A filename belongs to the **reference**, not to the content: the
            // same bytes can carry several filenames, which is why it lives here
            // rather than on the blobs branch
            "filename": eb.filename,
        }),

        // The replay drawer: which memories were injected on this turn.
        //
        // Only fact_id goes down, never the statement — the client syncs the
        // facts table anyway, so copying the body again here both wastes
        // bandwidth and leaves an unerasable duplicate behind once the fact is
        // redacted (which is exactly why this table deliberately stores only
        // ids)
        SyncPayload::EpisodeMemory(m) => json!({
            "id": m.id,
            "episode_id": m.episode_id,
            "fact_id": m.fact_id,
            "ordinal": m.ordinal,
            "channels": m.channels,
            "score": m.score,
            "device_id": m.device_id,
            "created_at": m.created_at.to_rfc3339(),
        }),

        SyncPayload::EpisodeToolCall(t) => json!({
            "id": t.id,
            "episode_id": t.episode_id,
            "ordinal": t.ordinal,
            "name": t.name,
            // Its own field, so the client never has to regex a filename out of
            // the wording of summary — that kind of extraction silently shows
            // the wrong file the moment the wording changes
            "path": t.path,
            "summary": t.summary,
            "ok": t.ok,
            "device_id": t.device_id,
            "created_at": t.created_at.to_rfc3339(),
        }),

        SyncPayload::BlobTranscript(t) => json!({
            "id": t.id,
            "blob_hash": t.blob_hash,
            "kind": t.kind.as_str(),
            "text": t.text,
            "span_start_ms": t.span_start_ms,
            "span_end_ms": t.span_end_ms,
            "transcribed_by": t.transcribed_by,
            "created_at": t.created_at.to_rfc3339(),
        }),

        SyncPayload::Entity(e) => json!({
            "id": e.id,
            "kind": e.kind,
            "name": e.name,
            "summary": e.summary,
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        SyncPayload::EntityMerge(m) => json!({
            "id": m.id,
            "from_entity": m.from_entity,
            "into_entity": m.into_entity,
            "reason": m.reason,
            "source_episode_id": m.source_episode_id,
            "created_at": m.created_at.to_rfc3339(),
        }),

        SyncPayload::Fact(f) => json!({
            "id": f.id,
            "subject_id": f.subject_id,
            "predicate": f.predicate,
            "object_text": f.object_text,
            "object_entity_id": f.object_entity_id,
            "statement": f.statement,
            "domain": f.domain,
            "confidence": f.confidence,
            // Both axes of the bitemporal record go down: the client needs them
            // for local time replay
            "valid_at": f.valid_at.map(|t| t.to_rfc3339()),
            "created_at": f.created_at.to_rfc3339(),
            "source_episode_id": f.source_episode_id,
            // Source channel and trust tier both go down: the client explains
            // "why this memory should be believed" in terms of its source, and
            // during a security incident it must be able to pick out that same
            // batch of sources locally. A null trust_tier appears only on rows
            // that predate the column (= unknown)
            "source_channel": f.source_channel.as_str(),
            "trust_tier": f.trust_tier,
            "extracted_by": f.extracted_by,
            "device_id": f.device_id,
        }),

        SyncPayload::FactEvent(e) => json!({
            "id": e.id,
            "fact_id": e.fact_id,
            "op": e.op.as_str(),
            "kind": e.kind.map(|k| k.as_str()),
            "invalid_at": e.invalid_at.map(|t| t.to_rfc3339()),
            "superseded_by": e.superseded_by,
            "actor": e.actor.as_str(),
            "reason": e.reason,
            "source_episode_id": e.source_episode_id,
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        SyncPayload::Summary(s) => json!({
            "id": s.id,
            "scope": s.scope.as_str(),
            "scope_key": s.scope_key,
            "text": s.text,
            "covers_from": s.covers_from.map(|t| t.to_rfc3339()),
            "covers_to": s.covers_to.map(|t| t.to_rfc3339()),
            "device_id": s.device_id,
            "created_at": s.created_at.to_rfc3339(),
        }),

        // Derivation lineage edges. These must go down, for the same reason as
        // redaction tombstones: on receiving a tombstone the client is obliged
        // to perform the equivalent local erasure, and **whether a derived
        // artifact can be erased depends on whether the client knows its
        // lineage**. Send the tombstone without the edges and the client can
        // erase the source but not the summaries and graph edges built on top of
        // it
        SyncPayload::Derivation(d) => json!({
            "id": d.id,
            "derived_kind": d.derived_kind.as_str(),
            "derived_id": d.derived_id,
            "source_kind": d.source_kind.as_str(),
            "source_id": d.source_id,
            "device_id": d.device_id,
            "created_at": d.created_at.to_rfc3339(),
        }),

        // Session lifecycle events. The client derives the final state itself
        // using the same state machine (take the last entry per dimension),
        // rather than waiting for the server to send down a "current title" —
        // that would require the server to push a state row after every event,
        // and such a row has no total order to speak of
        SyncPayload::SessionEvent(e) => json!({
            "id": e.id,
            "session_id": e.session_id,
            "op": e.op.as_str(),
            "title": e.title,
            // workspace is a **local machine** path, and once sent down it most
            // likely does not exist on the peer. Send it anyway: without it the
            // peer does not even know that this session has a workspace on some
            // other device, and the UI has no way to explain why the same
            // session has no file tools on this machine
            "workspace": e.workspace,
            // Which project this belongs to. **May point at an already-deleted
            // project** — deleting a project deliberately does not cascade a
            // rewrite over session events. The client does what the server does:
            // while computing the final state, treat a dangling membership as
            // ungrouped
            "project_id": e.project_id,
            "actor": e.actor.as_str(),
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        // Project lifecycle events. Same arrangement as the session branch: the
        // client computes the final state itself with a state machine (existence
        // from the last create/delete, the name from the last create/rename)
        // rather than waiting for the server to send down a "current project
        // list" — which has no total order to speak of
        SyncPayload::ProjectEvent(e) => json!({
            "id": e.id,
            "project_id": e.project_id,
            "op": e.op.as_str(),
            "name": e.name,
            "actor": e.actor.as_str(),
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        // Redaction tombstones must go down: on receiving one the client is
        // obliged to perform the equivalent local erasure (the propagation
        // obligation in memory.md §9)
        SyncPayload::Redaction(r) => json!({
            "id": r.id,
            "target_kind": r.target_kind.as_str(),
            "target_id": r.target_id,
            "mode": r.mode.as_str(),
            "reason": r.reason,
            "actor": r.actor,
            "device_id": r.device_id,
            "created_at": r.created_at.to_rfc3339(),
        }),
    }
}
