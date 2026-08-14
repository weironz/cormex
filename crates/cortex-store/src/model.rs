//! 各表的行结构体、写入输入与受约束列的枚举。
//!
//! 三条约定：
//!
//! 1. **`tsv` 不出现在任何结构体里。** 它是纯派生的检索产物，Rust 侧没有对应
//!    类型，读回来也无用。写入侧改为传 `tsv_source`（已分词、空格分隔的词串），
//!    由 SQL 里的 `to_tsvector('simple', $n)` 与主行同事务落库 ——
//!    docs/memory.md §八、§九 要求 tsv 永不异步补写。
//! 2. **`embedding` 保留。** 它是行内容的一部分：多端同步要下发它，
//!    换 embedding 模型要按 `embedding_model` 分批回填它。
//! 3. **有 CHECK 约束的列用枚举，开放的列用 `String`。**
//!    `entities.kind` 与 `facts.predicate` 在 schema 中刻意不枚举
//!    （枚举则新领域进不来），这里也就不能枚举。
//!
//! 主键在**写入侧**用 [`Id`]（编译期保证形如 ULID），在**读出侧**用 `String`
//! （数据库列是 `ulid` 域，本质是文本；读回来再解析只会平添失败路径）。
//! 调用方需要 [`Id`] 时自行 `parse()`。

use chrono::{DateTime, Utc};
use cortex_core::Id;
use pgvector::Vector;
use sqlx::FromRow;

/// 表名常量。`sync_log.table_name` 与 [`crate::SyncPayload`] 共用这一组字面量。
///
/// # 为什么用宏生成，而不是直接写十三个 `const`
///
/// 漏一支的后果不是「这张表同步不了」，是**整条 `/sync` 断掉**：
/// `load_payloads` 撞上不认识的表名会返回 `UnknownTable`，那一批拉取整个
/// 失败，客户端游标从此卡死在它前面 —— 而且只在那张表**第一次被写**时
/// 才发作，可能是上线几周之后。
///
/// 宏顺带生成 [`table::ALL`]，`tests/sync_coverage.rs` 拿它逐个撞
/// `load_payloads`。于是加表的人只要加了常量，忘记加 match 分支就会
/// **当场红**，而不是等用户的游标卡死。（那条测试自己也做过故障注入验证：
/// 塞一个假表名进列表，它确实变红。）
///
/// 单靠人写 `ALL` 不行 —— 那只是把「容易忘的地方」从一处挪到两处。
pub mod table {
    macro_rules! tables {
        ($($name:ident = $lit:literal),+ $(,)?) => {
            $(pub const $name: &str = $lit;)+
            /// 全部会出现在 `sync_log.table_name` 里的表名。
            pub const ALL: &[&str] = &[$($lit),+];
        };
    }

    tables! {
        EPISODES          = "episodes",
        BLOBS             = "blobs",
        EPISODE_BLOBS     = "episode_blobs",
        EPISODE_MEMORIES  = "episode_memories",
        EPISODE_TOOL_CALLS = "episode_tool_calls",
        BLOB_TRANSCRIPTS  = "blob_transcripts",
        ENTITIES          = "entities",
        ENTITY_MERGES     = "entity_merges",
        FACTS             = "facts",
        FACT_EVENTS       = "fact_events",
        SUMMARIES         = "summaries",
        DERIVATIONS       = "derivations",
        REDACTIONS        = "redactions",
        SESSION_EVENTS    = "session_events",
        PROJECT_EVENTS    = "project_events",
    }

    // ── 刻意**不**在上面那个宏里的两张表 ──────────────────────
    //
    // `fact_embeddings` / `entity_embeddings` 是换模型时补出来的向量
    // （见 migrations/20260808000001）。它们**永不进 sync_log**，
    // 所以也不该出现在 `ALL` 里 —— `ALL` 的定义就是「全部会出现在
    // sync_log.table_name 里的表名」。
    //
    // 为什么不同步：`sync_payload.rs` 的模块注释早就把理由写死了 ——
    // 「tsv 与 embedding 是服务端派生列，不进同步 payload……
    //  **换 embedding 模型时会全量变化，白白撑爆增量同步**」。
    // 那句话说的正是这件事。十万条事实的一次回填会推出十万条 sync_log，
    // 而每个客户端拿到的载荷里没有一个字段是它用得上的。
    //
    // 代价是它们不能走 `Guarded::insert_row`（那条通道强制写 sync_log），
    // 必须走 `insert_derived_row`。那个方法的注释里写了它的适用边界。
    pub const FACT_EMBEDDINGS: &str = "fact_embeddings";
    pub const ENTITY_EMBEDDINGS: &str = "entity_embeddings";
}

/// `facts` 表的全部列。四路召回、三条回放召回、按主键 / 主语 / 宾语 / 出处
/// 取事实，以及同步载荷，全都要原样取回。
///
/// 抄十几遍的下场是加一列时漏改其中一处 —— 而漏改的报错是运行时的
/// 「`FromRow` 找不到列 x」，且只在恰好走到那一条查询时才响。
/// 本次加 `source_channel` / `trust_tier` 两列时，query.rs 里那五份手抄
/// 副本正好演示了这个风险，于是一并收编到这里。
macro_rules! fact_columns {
    () => {
        fact_columns!("")
    };
    // 有 JOIN 的查询必须带表别名前缀，否则 `id` 会与被 JOIN 的一侧撞名
    ($p:literal) => {
        concat!(
            $p,
            "id, ",
            $p,
            "subject_id, ",
            $p,
            "predicate, ",
            $p,
            "object_text, ",
            $p,
            "object_entity_id, ",
            $p,
            "statement, ",
            $p,
            "embedding, ",
            $p,
            "embedding_model, ",
            $p,
            "domain, ",
            $p,
            "confidence, ",
            $p,
            "valid_at, ",
            $p,
            "source_episode_id, ",
            $p,
            "source_channel, ",
            $p,
            "trust_tier, ",
            $p,
            "extracted_by, ",
            $p,
            "device_id, ",
            $p,
            "created_at"
        )
    };
}

pub(crate) use fact_columns;

// ══════════════════════════════════════════════════════════
//  受 CHECK 约束的列 —— 以 TEXT 存储的枚举
// ══════════════════════════════════════════════════════════

/// 生成一个以 TEXT 存储的枚举，连同 sqlx 的 `Type` / `Encode` / `Decode`。
///
/// 不用 `#[derive(sqlx::Type)]`：那套宏面向 Postgres 原生 ENUM 类型，
/// 而 schema 里这些列是带 CHECK 的 TEXT。
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// 落库时使用的字面量，与 migration 里的 CHECK 一一对应。
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text ),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::StoreError;

            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                match s {
                    $( $text => Ok(Self::$variant), )+
                    other => Err($crate::StoreError::UnknownVariant {
                        ty: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl sqlx::Type<sqlx::Postgres> for $name {
            fn type_info() -> sqlx::postgres::PgTypeInfo {
                <str as sqlx::Type<sqlx::Postgres>>::type_info()
            }

            fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                <str as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            }
        }

        impl sqlx::Encode<'_, sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut sqlx::postgres::PgArgumentBuffer,
            ) -> std::result::Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                <&str as sqlx::Encode<'_, sqlx::Postgres>>::encode(self.as_str(), buf)
            }
        }

        impl<'r> sqlx::Decode<'r, sqlx::Postgres> for $name {
            fn decode(
                value: sqlx::postgres::PgValueRef<'r>,
            ) -> std::result::Result<Self, sqlx::error::BoxDynError> {
                let raw = <&'r str as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
                Ok(raw.parse()?)
            }
        }
    };
}

text_enum! {
    /// `episodes.role`
    pub enum Role {
        User => "user",
        Assistant => "assistant",
        Tool => "tool",
        System => "system",
    }
}

text_enum! {
    /// `blob_transcripts.kind` —— 媒体转录的来源 pipeline
    pub enum TranscriptKind {
        Asr => "asr",
        VisionCaption => "vision_caption",
        Ocr => "ocr",
        FrameCaption => "frame_caption",
    }
}

text_enum! {
    /// `fact_events.op`
    ///
    /// `Invalidate` / `Revoke` 是**状态**事件，末态由最后一条决定；
    /// `Flag` 只是批注（「矛盾待人工确认」），不改变状态。
    pub enum FactOp {
        Invalidate => "invalidate",
        Revoke => "revoke",
        Flag => "flag",
    }
}

text_enum! {
    /// `fact_events.kind` —— 失效原因。四者时间语义不同，混用会让认知回放出错。
    pub enum InvalidationKind {
        /// 曾经为真，世界变了。`superseded_by` 必填。
        Superseded => "superseded",
        /// 从未为真，抽取错误。
        Corrected => "corrected",
        /// 用户主动删除，不对真值表态。
        Retracted => "retracted",
        /// 曾经为真，到期失效。
        Expired => "expired",
    }
}

text_enum! {
    /// `fact_events.actor`
    pub enum Actor {
        System => "system",
        User => "user",
    }
}

text_enum! {
    /// `session_events.op`
    ///
    /// 四个互相独立的维度，各自是一台「末态由最后一条决定」的状态机：
    /// `Rename` 管标题，`Archive` / `Unarchive` 管归档，
    /// `BindWorkspace` / `UnbindWorkspace` 管工作区，
    /// `MoveToProject` / `RemoveFromProject` 管所属项目。
    ///
    /// **归档不是删除**：归档只把会话从默认列表隐藏，消息与派生记忆一概不动，
    /// 随时可 `Unarchive` 恢复。真正销毁内容的是 [`RedactionMode`]，
    /// 它要显式触发、二次确认、留墓碑（docs/memory.md §十一）。
    pub enum SessionOp {
        Rename => "rename",
        Archive => "archive",
        Unarchive => "unarchive",
        BindWorkspace => "bind_workspace",
        UnbindWorkspace => "unbind_workspace",
        MoveToProject => "move_to_project",
        RemoveFromProject => "remove_from_project",
        SetRuntime => "set_runtime",
    }
}

text_enum! {
    /// 这个会话在**哪儿**跑。
    ///
    /// # 为什么它必须写在会话上
    ///
    /// 同一个会话在两端各自跑，而两端指着完全不同的文件系统 —— Web 是云端
    /// 容器的 `/workspace`，桌面端是本机某个目录。不记下来的话，用户在 A 端
    /// 让 agent 写的文件，到 B 端就「不存在」，而 agent 说的是实话。
    ///
    /// 调研过的四家（Claude Code / Codex / Cursor / OpenHands）没有一家允许
    /// 同一个会话在两端各跑各的：**执行环境是会话身份的一部分**。
    ///
    /// # 为什么 Local 不带「哪一台设备」
    ///
    /// 链路上没有客户端设备身份（`device_id` 盖的是 cortexd 自己的章）。
    /// 但那个身份不必引入：本地绑定存在 `workspaces.json`，天然只在那一台
    /// 机器上，于是「这台机器有没有这个会话的绑定」本身就是设备检查 ——
    /// 而且比一个 id 更硬，id 在重装 / 克隆之后会骗人。
    pub enum SessionRuntime {
        /// 云端容器工作区。**默认**，也是唯一处处可续的那个。
        Cloud => "cloud",
        /// 钉在某台机器的本机目录上。别的设备打开它只能看，不能续。
        Local => "local",
    }
}

text_enum! {
    /// `project_events.op`
    ///
    /// 两台状态机共用一张表：`Create` / `Delete` 决定项目还在不在，
    /// `Create` / `Rename` 决定它此刻叫什么。`Create` 同时是两台机的起点。
    ///
    /// **刻意没有 archive**：项目是个轻量分组，再加一层「已归档的项目」
    /// 只会让界面多一个没人用的过滤器。理由见
    /// `migrations/20260812000003_projects.sql` 的表头。
    ///
    /// **`Delete` 删的是分组，不是内容**：项目从列表消失，里面的会话变成
    /// 未分组，消息与派生记忆一概不动。
    pub enum ProjectOp {
        Create => "create",
        Rename => "rename",
        Delete => "delete",
    }
}

text_enum! {
    /// `facts.source_channel` —— 读出侧的完整取值集。
    ///
    /// 写入侧用的是 [`FactSource`]，它**不含** [`Self::UnknownLegacy`]：
    /// 那个取值只属于加列之前的存量行，新写入必须说清楚自己从哪来。
    pub enum SourceChannel {
        /// 用户亲口说的
        UserStated => "user_stated",
        /// 模型从对话里推断出来的
        Conversation => "conversation",
        /// 模型从已有事实派生出来的（如 crosslink 的跨域边）
        Derived => "derived",
        /// 本地执行的工具输出
        ToolOutput => "tool_output",
        /// 外部内容：网页 / 他人文档 / MCP。默认不进抽取
        External => "external",
        /// 加列之前的存量行。真实来源无法事后重建 —— 标「不知道」，
        /// 而不是伪造一个查询时会被当真的档位。见 migration 20260807000006
        UnknownLegacy => "unknown_legacy",
    }
}

/// 写入侧的来源通道。
///
/// 与 [`SourceChannel`] 分成两个类型，只为一件事：
/// **让「新写入被标成 unknown_legacy」在编译期就不可能**。
/// 存量行的「不知道」是一次性的历史债，不该成为新代码的懒惰出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    UserStated,
    Conversation,
    Derived,
    ToolOutput,
    External,
}

impl FactSource {
    /// 落库的通道名。
    #[must_use]
    pub const fn channel(self) -> SourceChannel {
        match self {
            Self::UserStated => SourceChannel::UserStated,
            Self::Conversation => SourceChannel::Conversation,
            Self::Derived => SourceChannel::Derived,
            Self::ToolOutput => SourceChannel::ToolOutput,
            Self::External => SourceChannel::External,
        }
    }

    /// 信任级，1 最高。
    ///
    /// 映射在 Rust 与 SQL 里各写了一遍，由 migration 20260807000006 的
    /// `facts_trust_tier_matches_channel` 做最终裁决：两处漂移会在插入时
    /// 直接 23514，而不是悄悄写进一个自相矛盾的组合。
    #[must_use]
    pub const fn trust_tier(self) -> i16 {
        match self {
            Self::UserStated => 1,
            // Derived 与 Conversation 同档：两者都是模型的推断，区别在输入。
            // ⚠️ Derived 现在不继承输入事实的档位（信任洗白，见 migration
            // 20260807000006 的注释）—— 那要等 derivations 被读侧用起来
            Self::Conversation | Self::Derived => 2,
            Self::ToolOutput => 3,
            Self::External => 4,
        }
    }
}

text_enum! {
    /// `derivations.derived_kind` —— 派生物一侧的种类。
    pub enum DerivedKind {
        Fact => "fact",
        Summary => "summary",
    }
}

text_enum! {
    /// `derivations.source_kind` —— 源一侧的种类。
    ///
    /// 比 [`DerivedKind`] 多一个 `Episode`：L0 原文只会当源，不会是谁的派生物。
    pub enum SourceKind {
        Episode => "episode",
        Fact => "fact",
        Summary => "summary",
    }
}

text_enum! {
    /// `summaries.scope`
    pub enum SummaryScope {
        Session => "session",
        Topic => "topic",
        Period => "period",
    }
}

text_enum! {
    /// `redactions.target_kind`
    pub enum RedactionTarget {
        Episode => "episode",
        Blob => "blob",
    }
}

text_enum! {
    /// `redactions.mode`
    pub enum RedactionMode {
        /// 清除库内全部派生落点，保留墓碑与审计链。
        Redact => "redact",
        /// 在 redact 基础上再删除对象存储中的 blob 及备份镜像。
        Purge => "purge",
    }
}

// ══════════════════════════════════════════════════════════
//  L0 原始层
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Episode {
    pub id: String,
    pub session_id: String,
    pub role: Role,
    pub content: serde_json::Value,
    pub text: Option<String>,
    pub domain: Option<String>,
    pub device_id: String,
    /// 事件发生时间（客户端时钟，经单调化处理）
    pub occurred_at: DateTime<Utc>,
    /// 服务端入库时间
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisode {
    pub id: Id,
    pub session_id: String,
    pub role: Role,
    /// 原始消息，含供应商特有的 thinking / reasoning 不透明块，无损保存。
    pub content: serde_json::Value,
    /// 从 `content` 提取的纯文本
    pub text: Option<String>,
    /// jieba 分词后、空格分隔的词串；落库时转成 `tsv`。`None` 则 `tsv` 为 NULL。
    pub tsv_source: Option<String>,
    pub domain: Option<String>,
    pub device_id: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Blob {
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBlob {
    /// SHA-256 十六进制小写。内容寻址 —— 同内容同 hash，天然幂等。
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    pub storage_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeBlob {
    pub episode_id: String,
    pub blob_hash: String,
    pub kind: Option<String>,
    /// 这次引用时的原始文件名。
    ///
    /// 属于**引用**而非内容：内容寻址下同一份字节可以有多个文件名
    /// （同一张图今天叫「设计稿.png」，下周转发过来叫「IMG_2043.png」）。
    /// `None` = 未知（老数据，或客户端没提供）。
    pub filename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeBlob {
    pub episode_id: Id,
    pub blob_hash: String,
    pub kind: Option<String>,
    /// 见 [`EpisodeBlob::filename`]
    pub filename: Option<String>,
}

impl NewEpisodeBlob {
    /// `sync_log.record_id` 的复合键约定：`episode_id:blob_hash`。
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.episode_id, self.blob_hash)
    }
}

/// 换 embedding 模型时给一条已有事实补出来的向量。
///
/// 不是就地改 `facts.embedding` —— CLAUDE.md 的 append-only 不给这条路，
/// 而且「回填期间双模型召回」要求同一条事实**同时**持有两个空间的向量，
/// 一个列装不下两个。见 `migrations/20260808000001_embedding_backfill.sql`。
#[derive(Debug, Clone)]
pub struct NewFactEmbedding {
    pub fact_id: Id,
    pub embedding_model: String,
    pub embedding: Vector,
}

impl NewFactEmbedding {
    /// `sync_log.record_id` 的复合键约定：`fact_id:embedding_model`。
    ///
    /// 模型名进 record_id 是必须的：同一条事实在不同模型下是**不同的行**，
    /// 只用 fact_id 会让第二次回填看起来像是第一次的重复。
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.fact_id, self.embedding_model)
    }
}

/// 同上，实体侧。
#[derive(Debug, Clone)]
pub struct NewEntityEmbedding {
    pub entity_id: Id,
    pub embedding_model: String,
    pub embedding: Vector,
}

impl NewEntityEmbedding {
    /// `sync_log.record_id` 的复合键约定：`entity_id:embedding_model`。
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.entity_id, self.embedding_model)
    }
}

/// 一条附件引用**连同它的内容元信息**。
///
/// [`EpisodeBlob`] 是裸表行，只有 hash；客户端拿它渲染出来的是
/// 「文档 · a1b2c3d4」。真正要显示的 `mime` 与 `size_bytes` 在 `blobs` 里，
/// 于是读路径统一走这个 JOIN 出来的结构 —— 而不是让每个调用方
/// 自己再查一次 `blobs`（那是 N+1，且各处会渲染出不一样的东西）。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeAttachment {
    pub episode_id: String,
    pub blob_hash: String,
    pub kind: Option<String>,
    pub filename: Option<String>,
    /// 由字节头嗅探得出，来自 `blobs.mime`
    pub mime: String,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct BlobTranscript {
    pub id: String,
    pub blob_hash: String,
    pub kind: TranscriptKind,
    pub text: String,
    pub embedding: Option<Vector>,
    /// 音视频转录的片段毫秒偏移；图片 / OCR 为 NULL
    pub span_start_ms: Option<i64>,
    pub span_end_ms: Option<i64>,
    pub transcribed_by: String,
    pub embedding_model: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBlobTranscript {
    pub id: Id,
    pub blob_hash: String,
    pub kind: TranscriptKind,
    pub text: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub span_start_ms: Option<i64>,
    pub span_end_ms: Option<i64>,
    pub transcribed_by: String,
    pub embedding_model: String,
}

// ══════════════════════════════════════════════════════════
//  L1 事实层
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub summary: Option<String>,
    /// 仅用于抽取期的实体消解，不参与四路召回
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEntity {
    pub id: Id,
    /// person / project / file / org / concept / tool …… 开放不枚举
    pub kind: String,
    pub name: String,
    pub summary: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EntityMerge {
    pub id: String,
    pub from_entity: String,
    pub into_entity: String,
    pub reason: Option<String>,
    pub source_episode_id: Option<String>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEntityMerge {
    pub id: Id,
    pub from_entity: Id,
    pub into_entity: Id,
    pub reason: Option<String>,
    pub source_episode_id: Option<Id>,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Fact {
    pub id: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_text: Option<String>,
    pub object_entity_id: Option<String>,
    pub statement: String,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub domain: Option<String>,
    pub confidence: f32,
    /// 【事件时间】何时开始为真。NULL = 未知或一直如此
    pub valid_at: Option<DateTime<Utc>>,
    pub source_episode_id: String,
    /// 这条事实从哪个通道进来
    pub source_channel: SourceChannel,
    /// 来源信任级，1 最高。
    ///
    /// `None` **只可能**出现在加列之前的存量行上（`unknown_legacy`）——
    /// 「不知道」不是信任标尺上的一个点，所以它是 NULL 而不是某个哨兵档位。
    pub trust_tier: Option<i16>,
    pub extracted_by: String,
    pub device_id: String,
    /// 【系统时间】Cortex 何时知道这件事
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub id: Id,
    pub subject_id: Id,
    /// prefers / decided / owns / blocked_by …… 开放不枚举
    pub predicate: String,
    pub object_text: Option<String>,
    pub object_entity_id: Option<Id>,
    pub statement: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub domain: Option<String>,
    /// 抽取器对「该事实忠实反映原文」的一次性打分，此后不可变
    pub confidence: f32,
    pub valid_at: Option<DateTime<Utc>>,
    pub source_episode_id: Id,
    /// 来源通道。落库时连带写出 `trust_tier`（[`FactSource::trust_tier`]）。
    ///
    /// 没有默认值是刻意的：给它一个默认值就等于让「忘记表态」静默变成
    /// 「用户亲述」或「不知道」，而这一列存在的全部理由就是出事之后
    /// 还能按来源把东西挑出来。
    pub source: FactSource,
    /// `source_episode_id` **之外**的源。
    ///
    /// 单源派生（抽取器：一条 episode → 若干事实）留空即可 ——
    /// `source_episode_id NOT NULL` 已经是它的血缘。需要用到这里的是**多源**
    /// 派生：crosslink 的跨域边由两条事实推出，只记左边那条的出处会让
    /// 右边那条被 redact 时这行边悄悄活下来（见 `cortex_memory::crosslink`）。
    pub derived_from: Vec<ProvenanceRef>,
    pub extracted_by: String,
    pub device_id: String,
}

/// 一条源引用：`derivations` 表里源那一侧的 (kind, id)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProvenanceRef {
    pub kind: SourceKind,
    pub id: Id,
}

impl ProvenanceRef {
    /// 源是一条 L0 原始消息。
    #[must_use]
    pub const fn episode(id: Id) -> Self {
        Self {
            kind: SourceKind::Episode,
            id,
        }
    }

    /// 源是一条事实。
    #[must_use]
    pub const fn fact(id: Id) -> Self {
        Self {
            kind: SourceKind::Fact,
            id,
        }
    }

    /// 源是一条摘要（血缘可以多层：画像块 → 摘要 → episode）。
    #[must_use]
    pub const fn summary(id: Id) -> Self {
        Self {
            kind: SourceKind::Summary,
            id,
        }
    }
}

/// `derivations` 的表行。写入侧不单独拆 `New*`：这张表没有 tsv / embedding
/// 这类只写不读的列，而 `id` 与 `created_at` 由写入方法自己填。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Derivation {
    pub id: String,
    pub derived_kind: DerivedKind,
    pub derived_id: String,
    pub source_kind: SourceKind,
    pub source_id: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct FactEvent {
    pub id: String,
    pub fact_id: String,
    pub op: FactOp,
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<String>,
    pub actor: Actor,
    pub reason: Option<String>,
    pub source_episode_id: Option<String>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewFactEvent {
    pub id: Id,
    pub fact_id: Id,
    pub op: FactOp,
    /// `op = Invalidate` 时必填，其余必须为 `None`（schema CHECK 强制）
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    /// `kind = Superseded` 时必填，否则事实演化链断裂
    pub superseded_by: Option<Id>,
    pub actor: Actor,
    pub reason: Option<String>,
    pub source_episode_id: Option<Id>,
    pub device_id: String,
}

impl NewFactEvent {
    /// 系统自动失效：旧事实被新事实取代。
    #[must_use]
    pub fn superseded(
        fact_id: Id,
        superseded_by: Id,
        invalid_at: DateTime<Utc>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Invalidate,
            kind: Some(InvalidationKind::Superseded),
            invalid_at: Some(invalid_at),
            superseded_by: Some(superseded_by),
            actor: Actor::System,
            reason: None,
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// 用户删除一条记忆：不对真值表态，仅不再使用。
    #[must_use]
    pub fn retracted(
        fact_id: Id,
        invalid_at: DateTime<Utc>,
        reason: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Invalidate,
            kind: Some(InvalidationKind::Retracted),
            invalid_at: Some(invalid_at),
            superseded_by: None,
            actor: Actor::User,
            reason: Some(reason.into()),
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// 撤销上一次失效（恢复）。状态机语义：末态由最后一条状态事件决定。
    #[must_use]
    pub fn revoke(fact_id: Id, actor: Actor, device_id: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Revoke,
            kind: None,
            invalid_at: None,
            superseded_by: None,
            actor,
            reason: None,
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// 标记「矛盾待人工确认」。批注，不改变状态。
    #[must_use]
    pub fn flag(fact_id: Id, reason: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Flag,
            kind: None,
            invalid_at: None,
            superseded_by: None,
            actor: Actor::System,
            reason: Some(reason.into()),
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }
}

// ══════════════════════════════════════════════════════════
//  L2 摘要层
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Summary {
    pub id: String,
    pub scope: SummaryScope,
    pub scope_key: String,
    pub text: String,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub covers_from: Option<DateTime<Utc>>,
    pub covers_to: Option<DateTime<Utc>>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSummary {
    pub id: Id,
    pub scope: SummaryScope,
    pub scope_key: String,
    pub text: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub covers_from: Option<DateTime<Utc>>,
    pub covers_to: Option<DateTime<Utc>>,
    /// 这份摘要是从哪些东西摘出来的。**必须非空**
    /// （[`crate::WriteTxn::insert_summary`] 会拒绝空列表）。
    ///
    /// 摘要不像事实那样有一列 `source_episode_id NOT NULL` 兜底，没有这份
    /// 血缘就没有任何办法在源被 redact 之后找到它 —— 而 GDPR Art.17 的
    /// 擦除义务及于派生数据。一条摘不出源头的摘要是无法追责的僵尸记忆，
    /// 所以它宁可写不进去（见 docs/memory-content.md §5.3）。
    pub sources: Vec<ProvenanceRef>,
    pub device_id: String,
}

// ══════════════════════════════════════════════════════════
//  回放抽屉 —— 一轮对话「注入了什么、调用了什么」
//
//  只在内存里活着的归因等于没有：刷新一次抽屉就空了。
//  设计取舍（存 id 而非快照、锚在 user episode 上）见
//  migrations/20260807000005_episode_replay.sql 的表头注释。
// ══════════════════════════════════════════════════════════

/// `episode_memories` 的裸表行（同步下发用）。
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EpisodeMemory {
    pub id: String,
    pub episode_id: String,
    pub fact_id: String,
    pub ordinal: i32,
    pub channels: Option<Vec<String>>,
    pub score: Option<f64>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeMemory {
    pub id: Id,
    /// 本轮的锚点 —— **user 那条 episode**（assistant 那条在出错时不落库）
    pub episode_id: Id,
    pub fact_id: Id,
    /// 注入顺序，0 起
    pub ordinal: i32,
    /// 命中的召回路（bm25 / vector / graph / episode …）
    pub channels: Vec<String>,
    pub score: Option<f64>,
    pub device_id: String,
}

/// 回放一条注入记忆时**当下**看到的样子。
///
/// 不是裸表行：`statement` 与失效状态来自 `facts` / `fact_status` 的 JOIN。
/// 存的是 id、渲染的是现状 —— 于是一条事实被 redact 之后，抽屉里跟着变成
/// 占位符，不需要再去清一遍这张表。
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InjectedMemory {
    pub episode_id: String,
    pub fact_id: String,
    pub ordinal: i32,
    pub channels: Option<Vec<String>>,
    pub score: Option<f64>,
    /// `None` = 这条事实的行已经不在了。理论上不该发生（append-only，
    /// 且有外键），出现即说明有人绕过本层删了数据 —— 界面应显示
    /// 「引用了一条已不可见的记忆」而不是把这一行整个藏起来
    pub statement: Option<String>,
    pub domain: Option<String>,
    pub source_episode_id: Option<String>,
    /// 注入之后这条事实**已被失效**。回放要如实说出来：
    /// 「当时依据的这条，现在已经不成立了」正是审计最想看到的信息
    pub invalidated: bool,
}

/// `episode_tool_calls` 的表行。写入侧与读出侧共用形状，
/// 因此不像别的表那样拆 `New*`——它没有 tsv / embedding 这类只写不读的列。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeToolCall {
    pub id: String,
    pub episode_id: String,
    pub ordinal: i32,
    pub name: String,
    /// 这次调用碰的文件路径；没碰文件的工具为 `None`
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    pub device_id: String,
    /// 这次写入改了什么（统一 diff，已截断）。`None` = 没有可看的改动。
    pub diff: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeToolCall {
    pub id: Id,
    pub episode_id: Id,
    pub ordinal: i32,
    pub name: String,
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    pub device_id: String,
    /// 这次写入改了什么（统一 diff，**已截断**）。
    ///
    /// 截断在 agent 侧（`cortex_agent::diff`）就做完了，那里还能在末尾写明
    /// 「其余未显示」。这一列会随 `sync_log` 下发到所有设备，
    /// 所以 migration 上还有一条 8 KiB 的 CHECK 兜底。
    pub diff: Option<String>,
}

/// `episode_tool_calls.summary` 的字符数上限，与 migration 里的 CHECK 一致。
///
/// 常量在这里而不是让上层写字面量：两处漂移的症状是「工具摘要一长，
/// 整个写事务回滚」——而那个事务里还带着本轮的记忆归因。
pub const TOOL_SUMMARY_MAX_CHARS: usize = 2048;

/// `episode_tool_calls.diff` 的字符数上限，与 migration 里的 CHECK 一致。
///
/// 比 summary 大一个量级：一份 diff 本来就是几十行起步。但仍然要有上限 ——
/// 这一列会随 `sync_log` **下发到所有设备**，一次几百行的重构轻松几十 KB，
/// 乘以设备数、乘以每轮多个 `write_file`，就是同步通道上一笔谁都没预算过
/// 的流量。
///
/// 与 `cortex_agent::diff` 那边的上限**各管各的**：那边管「人读得完」，
/// 这边管「入库与同步扛得住」。两个数将来完全可能各自变动，
/// 所以不共用一个常量。
pub const TOOL_DIFF_MAX_CHARS: usize = 8192;

/// `episode_tool_calls.name` 的字符数上限，与 migration 里的 CHECK 一致。
pub const TOOL_NAME_MAX_CHARS: usize = 128;

/// `episode_tool_calls.path` 与 `session_events.workspace` 共用的路径上限。
pub const TOOL_PATH_MAX_CHARS: usize = 4096;

/// `episode_blobs.filename` 的字符数上限，与 migration 里的 CHECK 一致。
pub const ATTACHMENT_FILENAME_MAX_CHARS: usize = 255;

// ══════════════════════════════════════════════════════════
//  会话生命周期
// ══════════════════════════════════════════════════════════

/// `session_events.title` 的字符数上限，与 migration 里的 CHECK 一致。
///
/// 常量放在这里而不是让上层各写一个字面量：两处一旦漂移，症状是
/// 「客户端以为改名成功，服务端却回 500」——数据库 CHECK 的报错到不了用户眼前。
pub const SESSION_TITLE_MAX_CHARS: usize = 200;

/// `session_events.workspace` 的字符数上限，与 migration 里的 CHECK 一致。
///
/// **定义在 [`cortex_core`]**，因为校验这个上限的
/// [`cortex_agent::workspace::validate`] 跑在两侧：cortexd（服务端目录）与
/// 本地 agent（用户自己机器上的目录），而后者不该为了一个数字依赖数据库层。
/// 这里 re-export 保持 `cortex_store::WORKSPACE_PATH_MAX_CHARS` 可用 ——
/// 它与 migration 的 CHECK 对齐，那件事仍然是存储层的责任。
pub use cortex_core::WORKSPACE_PATH_MAX_CHARS;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionEvent {
    pub id: String,
    pub session_id: String,
    pub op: SessionOp,
    /// `op = Rename` 时非空
    pub title: Option<String>,
    /// `op = BindWorkspace` 时非空。**本机绝对路径** ——
    /// 同步到别的设备上可能根本不存在，客户端应把「不存在」当作未绑定。
    pub workspace: Option<String>,
    /// `op = MoveToProject` 时非空。指向的项目**可能已经被删了** ——
    /// 删项目刻意不级联改写会话事件，末态视图会把悬挂的绑定当作未分组。
    pub project_id: Option<String>,
    /// `op = SetRuntime` 时非空。
    pub runtime: Option<SessionRuntime>,
    pub actor: Actor,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub id: Id,
    pub session_id: String,
    pub op: SessionOp,
    /// `op = Rename` 时必填，其余必须为 `None`（schema CHECK 强制）
    pub title: Option<String>,
    /// `op = BindWorkspace` 时必填，其余必须为 `None`（schema CHECK 强制）
    pub workspace: Option<String>,
    /// `op = MoveToProject` 时必填，其余必须为 `None`（schema CHECK 强制）
    pub project_id: Option<String>,
    /// `op = SetRuntime` 时必填，其余必须为 `None`（schema CHECK 强制）
    pub runtime: Option<SessionRuntime>,
    pub actor: Actor,
    pub device_id: String,
}

impl NewSessionEvent {
    fn bare(session_id: &str, op: SessionOp, actor: Actor, device_id: &str) -> Self {
        Self {
            id: Id::new(),
            session_id: session_id.to_owned(),
            op,
            title: None,
            workspace: None,
            project_id: None,
            runtime: None,
            actor,
            device_id: device_id.to_owned(),
        }
    }

    /// 用户给会话起了个名字。末态由最后一条 `rename` 决定 ——
    /// 不做「撤销栈」，`[rename A, rename B]` 之后就是 B。
    #[must_use]
    pub fn rename(session_id: &str, title: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            title: Some(title.to_owned()),
            ..Self::bare(session_id, SessionOp::Rename, actor, device_id)
        }
    }

    /// 归档 —— 从默认列表隐藏，**不删除任何东西**。
    ///
    /// 用户口中的「删除会话」在 append-only 体系里就是这个操作：消息、附件、
    /// 已抽取的事实全都原样留着，只是不再出现在列表里。真要销毁内容得走
    /// `redactions` 的 redact / purge，语义不同、要二次确认（docs/memory.md §十一）。
    #[must_use]
    pub fn archive(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::Archive, actor, device_id)
    }

    /// 撤销归档。
    #[must_use]
    pub fn unarchive(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::Unarchive, actor, device_id)
    }

    /// 把会话绑到一个本机目录上。
    ///
    /// `workspace` 必须是**已由服务端校验过**的绝对路径（存在、是目录、
    /// 不是系统目录）。存储层不做校验：它没有「什么算系统目录」这个概念，
    /// 那是部署形态的知识，属于 cortexd。
    #[must_use]
    pub fn bind_workspace(
        session_id: &str,
        workspace: &str,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            workspace: Some(workspace.to_owned()),
            ..Self::bare(session_id, SessionOp::BindWorkspace, actor, device_id)
        }
    }

    /// 解绑工作区 —— 会话退回纯聊天，文件工具从模型的工具目录里消失。
    #[must_use]
    pub fn unbind_workspace(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::UnbindWorkspace, actor, device_id)
    }

    /// 声明这个会话在哪儿跑。见 [`SessionRuntime`]。
    ///
    /// **与 `bind_workspace` 是两件事**，虽然它们常常一起发生：前者说的是
    /// 「哪个目录」，这个说的是「哪一侧的执行环境」。绑本机目录的会话必然是
    /// `Local`，但反过来不成立 —— 一个 `Local` 会话可以此刻还没绑目录
    /// （用户刚在这台机器上新建了它，还没选）。
    #[must_use]
    pub fn set_runtime(
        session_id: &str,
        runtime: SessionRuntime,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            ..Self::bare(session_id, SessionOp::SetRuntime, actor, device_id)
        }
    }

    /// 把会话移进某个项目。再移一次就是换项目 —— 一个会话同时只属于一个项目。
    ///
    /// 存储层**不校验项目存不存在**：那需要一次读，而写事务持着 advisory lock、
    /// 纪律要求它短小纯写（见 [`crate::txn`]）。校验属于 cortexd，在进事务前做。
    /// 即使漏了校验也不会脏掉视图：`session_state` 把指向已删除项目的绑定
    /// 当作未分组。
    #[must_use]
    pub fn move_to_project(
        session_id: &str,
        project_id: &str,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            project_id: Some(project_id.to_owned()),
            ..Self::bare(session_id, SessionOp::MoveToProject, actor, device_id)
        }
    }

    /// 把会话移出项目，退回未分组。
    ///
    /// 与「删项目」不是一回事：这一条只动这一个会话，而删项目是解散整个分组。
    #[must_use]
    pub fn remove_from_project(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::RemoveFromProject, actor, device_id)
    }
}

// ══════════════════════════════════════════════════════════
//  项目生命周期
// ══════════════════════════════════════════════════════════

/// `project_events.name` 的字符数上限，与 migration 里的 CHECK 一致。
///
/// 常量在这里而不是让上层写字面量：两处漂移的症状是「客户端以为改名成功，
/// 服务端却回 500」—— 数据库 CHECK 的报错到不了用户眼前。
pub const PROJECT_NAME_MAX_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ProjectEvent {
    pub id: String,
    pub project_id: String,
    pub op: ProjectOp,
    /// `op = Create | Rename` 时非空
    pub name: Option<String>,
    pub actor: Actor,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewProjectEvent {
    pub id: Id,
    pub project_id: String,
    pub op: ProjectOp,
    /// `op = Create | Rename` 时必填，`Delete` 必须为 `None`（schema CHECK 强制）
    pub name: Option<String>,
    pub actor: Actor,
    pub device_id: String,
}

impl NewProjectEvent {
    fn bare(project_id: &str, op: ProjectOp, actor: Actor, device_id: &str) -> Self {
        Self {
            id: Id::new(),
            project_id: project_id.to_owned(),
            op,
            name: None,
            actor,
            device_id: device_id.to_owned(),
        }
    }

    /// 建一个项目。`project_id` 由调用方生成（通常是一个新 ULID）。
    ///
    /// **同一个 id 再 create 一次是「撤销删除」**，不是错误：末态由最后一条
    /// 生命周期事件决定，于是重建之后原来那些会话的归属原样回来 ——
    /// 这正是删项目刻意不级联改写会话事件换来的东西。
    #[must_use]
    pub fn create(project_id: &str, name: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            ..Self::bare(project_id, ProjectOp::Create, actor, device_id)
        }
    }

    /// 改名。末态由最后一条 `rename`（或 `create`）决定，不做撤销栈。
    #[must_use]
    pub fn rename(project_id: &str, name: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            ..Self::bare(project_id, ProjectOp::Rename, actor, device_id)
        }
    }

    /// 删项目 —— **解散分组，不动内容**。
    ///
    /// 里面的会话变成未分组，消息、附件、已抽取的事实一条不少。
    /// 真要销毁内容得走 `redactions` 的 redact / purge，那是另一条路、
    /// 要二次确认（docs/memory.md §十一）。
    #[must_use]
    pub fn delete(project_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(project_id, ProjectOp::Delete, actor, device_id)
    }
}

// ══════════════════════════════════════════════════════════
//  抹除记录
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Redaction {
    pub id: String,
    pub target_kind: RedactionTarget,
    /// episode 时是 ULID，blob 时是 SHA-256
    pub target_id: String,
    pub mode: RedactionMode,
    pub reason: String,
    pub actor: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewRedaction {
    pub id: Id,
    pub target_kind: RedactionTarget,
    pub target_id: String,
    pub mode: RedactionMode,
    /// schema 要求必填 —— 抹除必须留下理由
    pub reason: String,
    pub actor: String,
    pub device_id: String,
}

// ══════════════════════════════════════════════════════════
//  视图
// ══════════════════════════════════════════════════════════

/// `fact_status` 视图：一条事实最近一次**状态**事件（`flag` 不参与）。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct FactStatus {
    pub fact_id: String,
    pub op: FactOp,
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<String>,
    pub actor: Actor,
    pub decided_at: DateTime<Utc>,
}

/// `session_state` 视图：一个会话四个维度各自的末态。
///
/// 四个维度**互相独立**地取各自最后一条事件 —— `[archive, rename]` 之后
/// 会话既有了新标题，也仍然处于归档中。用「整表最后一条」判定会得出
/// 「改个名就自动出档了」这种谁也没要求过的行为。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionState {
    pub session_id: String,
    /// 用户设置的标题。`None` = 从未改过名，上层应回落到「首条用户消息派生」。
    pub title: Option<String>,
    pub archived: bool,
    /// 绑定的本机目录。`None` = 未绑定，该会话是纯聊天（没有文件工具）。
    pub workspace: Option<String>,
    /// 所属项目。`None` = 未分组。
    ///
    /// **指向已删除项目的绑定在视图里就已经变成 `None`** —— 删项目不级联
    /// 改写会话事件，悬挂的绑定是常态，判定收敛在 SQL 里一处。
    pub project_id: Option<String>,
    /// 这个会话在哪儿跑。**视图里已经把 NULL 折成 `Cloud`** ——
    /// 缺省定在 SQL 一处，而不是让每个调用方各判一遍「没有事件算什么」。
    pub runtime: SessionRuntime,
    /// 五个维度中最近一次事件的时间
    pub decided_at: DateTime<Utc>,
}

/// `project_state` 视图 + 会话计数：项目列表的一行。
///
/// `session_count` 不是视图的一部分（视图只回答「这个项目此刻叫什么」），
/// 而是列表查询顺手聚合出来的 —— 见 [`crate::Store::projects`]。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Project {
    pub project_id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// 这个项目下有多少个**会显示在列表里**的会话。口径见
    /// [`crate::Store::projects`] 的文档。
    pub session_count: i64,
}

/// `canonical_entities` 视图：实体经别名合并后的最终归属。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct CanonicalEntity {
    pub id: String,
    pub canonical: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enum_roundtrips_through_str() {
        for op in [FactOp::Invalidate, FactOp::Revoke, FactOp::Flag] {
            assert_eq!(op.as_str().parse::<FactOp>().expect("应能解析回来"), op);
        }
    }

    #[test]
    fn unknown_variant_is_rejected() {
        let err = "nope".parse::<Role>().expect_err("未知取值应当报错");
        assert!(matches!(err, crate::StoreError::UnknownVariant { .. }));
    }

    #[test]
    fn session_event_constructors_respect_the_schema_checks() {
        // schema 的 CHECK 是两侧的：rename 必须有 title，且**只有** rename
        // 能有 title。构造器写错会在集成测试里变成一句 SQLSTATE 23514，
        // 这里先用单测把意图钉住
        let r = NewSessionEvent::rename("s", "新标题", Actor::User, "dev");
        assert_eq!(r.title.as_deref(), Some("新标题"));
        assert!(r.workspace.is_none(), "rename 不该携带 workspace");

        let b = NewSessionEvent::bind_workspace("s", "/tmp/x", Actor::User, "dev");
        assert_eq!(b.workspace.as_deref(), Some("/tmp/x"));
        assert!(b.title.is_none(), "bind_workspace 不该携带 title");

        let m = NewSessionEvent::move_to_project("s", "proj", Actor::User, "dev");
        assert_eq!(m.project_id.as_deref(), Some("proj"));
        assert!(
            m.title.is_none() && m.workspace.is_none(),
            "move_to_project 不该携带 title / workspace"
        );

        for e in [
            NewSessionEvent::archive("s", Actor::User, "dev"),
            NewSessionEvent::unarchive("s", Actor::User, "dev"),
            NewSessionEvent::unbind_workspace("s", Actor::User, "dev"),
            NewSessionEvent::remove_from_project("s", Actor::User, "dev"),
        ] {
            assert!(
                e.title.is_none() && e.workspace.is_none() && e.project_id.is_none(),
                "{} 是纯状态事件，不该携带任何载荷",
                e.op
            );
        }
    }

    #[test]
    fn project_event_constructors_respect_the_schema_checks() {
        // 与 session_events 同一套两侧 CHECK：create / rename 必须有 name，
        // 且**只有**它们能有 name。构造器写错会在集成测试里变成一句
        // SQLSTATE 23514，这里先用单测把意图钉住
        for e in [
            NewProjectEvent::create("p", "新项目", Actor::User, "dev"),
            NewProjectEvent::rename("p", "改过的名", Actor::User, "dev"),
        ] {
            assert!(e.name.is_some(), "{} 必须携带 name", e.op);
        }

        let d = NewProjectEvent::delete("p", Actor::User, "dev");
        assert!(
            d.name.is_none(),
            "delete 是纯状态事件，带上 name 会让视图表现为「删一下名字就变了」"
        );
    }

    #[test]
    fn episode_blob_record_id_uses_colon() {
        let link = NewEpisodeBlob {
            episode_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("合法 ULID"),
            blob_hash: "a".repeat(64),
            kind: None,
            filename: None,
        };
        assert_eq!(
            link.record_id(),
            format!("01ARZ3NDEKTSV4RRFFQ69G5FAV:{}", "a".repeat(64))
        );
    }
}
