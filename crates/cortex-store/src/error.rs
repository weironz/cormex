//! 存储层错误。
//!
//! 本层不吞错：sqlx 的原始错误原样携带，边界处（HTTP / CLI）再收敛到
//! [`cortex_core::CortexError`]。

use thiserror::Error;

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("数据库错误：{0}")]
    Sql(#[from] sqlx::Error),

    #[error("迁移失败：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("{ty} 不认识的取值：{value}")]
    UnknownVariant { ty: &'static str, value: String },

    /// `sync_log.table_name` 指向了本层不认识的表。
    /// 出现即说明有人绕过 [`crate::WriteTxn`] 直接写了库。
    #[error("sync_log 引用了未知的表：{0}")]
    UnknownTable(String),

    /// 派生物没有声明它的源。
    ///
    /// 不是「校验没过」这么简单：源被 redact 之后，找不到血缘的派生物会
    /// 继续泄露已被擦除的内容，而且**事后补不出来**（谁摘的、摘了什么，
    /// 只有生成它的那一步知道）。所以宁可这条摘要写不进去。
    #[error("{kind} {id} 没有声明任何源 —— 派生物必须带血缘（docs/memory-content.md §5.3）")]
    MissingProvenance { kind: &'static str, id: String },

    /// 调用方给的东西不对（schema 名不合法、租户池满了）。
    ///
    /// 与 [`Self::Sql`] 分开是有用的：那些是「数据库那边出事了」，
    /// 该重试或者告警；这一类是「你这么问不对」，重试多少次都一样。
    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    /// 这次写失败是不是「主键 / 唯一约束已存在」。
    ///
    /// 判 SQLSTATE 而不是匹配错误消息：消息带着表名、约束名与 Postgres 的
    /// 本地化措辞，换个 locale 或改个约束名就不匹配了。23505 是 SQL 标准里的
    /// `unique_violation`，不会变。
    ///
    /// # 谁需要它
    ///
    /// 幂等写入。本地 agent 断网时把 episode 排进队列，联网后重放 ——
    /// 「服务端收下了」与「本地记下已刷出」之间崩一次就会重来一遍。
    /// 那是**正常路径**：认不出来就会报 500，客户端把一次成功的重放当成
    /// 失败，然后永远重试下去。
    ///
    /// 判定放在这一层而不是调用方：`sqlx::Error` 的形状是存储层的实现细节，
    /// 让 cortexd 为了摸一个 SQLSTATE 而依赖 sqlx，等于把整个数据库驱动
    /// 拉进依赖图里 —— 而依赖方向是严格单向的。
    #[must_use]
    pub fn is_duplicate_key(&self) -> bool {
        matches!(self, Self::Sql(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505"))
    }
}

impl From<StoreError> for cortex_core::CortexError {
    fn from(err: StoreError) -> Self {
        Self::Store(err.to_string())
    }
}
