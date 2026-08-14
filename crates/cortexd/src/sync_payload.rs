//! 同步载荷的对外序列化。
//!
//! 不给 `cortex_store::SyncPayload` 直接 derive `Serialize`，而是在这里
//! 显式映射，理由有二：
//!
//! 1. **`tsv` 与 `embedding` 是服务端派生列，不进同步 payload**
//!    （memory.md §八 的硬性规定）。它们体积大、客户端用不上，
//!    且换 embedding 模型时会全量变化，白白撑爆增量同步。
//! 2. 存储层的结构体是内部表示，让它直接决定线上协议，
//!    等于把每次内部重构都变成一次协议破坏。
//!
//! 代价是新增字段要在这里补一行。这是**故意的摩擦** ——
//! 它逼着人想清楚「这个字段该不该给客户端」。

use cortex_store::SyncPayload;
use serde_json::{Value, json};

/// 把存储层的载荷转成下发给客户端的 JSON。
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
            // 不下发 storage_key：客户端应通过 presigned URL 取对象，
            // 而不是拿着内部存储路径直连
            "created_at": b.created_at.to_rfc3339(),
        }),

        SyncPayload::EpisodeBlob(eb) => json!({
            "episode_id": eb.episode_id,
            "blob_hash": eb.blob_hash,
            "kind": eb.kind,
            // 文件名属于**引用**而非内容：同一份字节可以有多个文件名，
            // 所以它在这里而不是在 blobs 那一支里
            "filename": eb.filename,
        }),

        // 回放抽屉：这一轮注入了哪些记忆。
        //
        // 只下发 fact_id，不下发 statement —— 客户端本来就会同步到 facts 表，
        // 在这里再抄一份正文既浪费带宽，又会在事实被 redact 后留下一个
        // 抹不掉的副本（那正是这张表刻意只存 id 的理由）
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
            // 独立字段，客户端不必从 summary 的措辞里正则抠文件名 ——
            // 那种抠法在措辞一改时会静默显示错文件
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
            // 双时间轴都要给：客户端做本地回放需要它们
            "valid_at": f.valid_at.map(|t| t.to_rfc3339()),
            "created_at": f.created_at.to_rfc3339(),
            "source_episode_id": f.source_episode_id,
            // 来源通道与信任级都要给：客户端要按来源解释「这条记忆凭什么可信」，
            // 出安全事件时也要能在本地把同一批来源挑出来。
            // trust_tier 为 null 只出现在加列之前的存量行上（= 不知道）
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

        // 派生血缘边。必须下发，理由与 redaction 墓碑同源：客户端收到墓碑后
        // 有义务执行同等的本地清除，而**派生物清不清得掉，取决于它知不知道
        // 血缘**。只发墓碑不发边，客户端能清源、清不掉建立在源之上的摘要与图边
        SyncPayload::Derivation(d) => json!({
            "id": d.id,
            "derived_kind": d.derived_kind.as_str(),
            "derived_id": d.derived_id,
            "source_kind": d.source_kind.as_str(),
            "source_id": d.source_id,
            "device_id": d.device_id,
            "created_at": d.created_at.to_rfc3339(),
        }),

        // 会话生命周期事件。客户端按同一套状态机自己算末态（每个维度取
        // 最后一条），而不是等服务端下发一个「当前标题」——后者要求服务端
        // 在每次事件后再推一行状态，而那行状态本身没有全序可言
        SyncPayload::SessionEvent(e) => json!({
            "id": e.id,
            "session_id": e.session_id,
            "op": e.op.as_str(),
            "title": e.title,
            // workspace 是**本机**路径，下发出去多半在对端不存在。
            // 仍然要发：不发的话对端连「这个会话在别的设备上是有工作区的」
            // 都不知道，界面上没法解释为什么同一个会话在这台机器上没有文件工具
            "workspace": e.workspace,
            // 归属哪个项目。**可能指向一个已经被删的项目** —— 删项目刻意不
            // 级联改写会话事件，客户端与服务端一样，算末态时把悬挂的归属
            // 当作未分组即可
            "project_id": e.project_id,
            "actor": e.actor.as_str(),
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        // 项目生命周期事件。与会话那支同一套：客户端按状态机自己算末态
        // （存在与否取最后一条 create/delete，名字取最后一条 create/rename），
        // 而不是等服务端下发一份「当前项目列表」——后者没有全序可言
        SyncPayload::ProjectEvent(e) => json!({
            "id": e.id,
            "project_id": e.project_id,
            "op": e.op.as_str(),
            "name": e.name,
            "actor": e.actor.as_str(),
            "device_id": e.device_id,
            "created_at": e.created_at.to_rfc3339(),
        }),

        // 抹除墓碑必须下发：客户端收到后有义务执行同等的本地清除
        // （memory.md §九 的传播义务）
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
