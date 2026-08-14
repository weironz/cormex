//! 配额：一把共享的 LLM key 要能分给多个人，且不被某一个人烧光。
//!
//! # 为什么这件事在开放注册之后变紧急
//!
//! 单用户时「配额」是个空概念 —— 花的是自己的钱。开放注册之后，
//! 服务端那把 key 对每个注册的人都可用，而生产是一台 2 核 3.5 GB 的机器：
//! 一个人跑一次三年历史导入就是 6000 次抽取调用。
//!
//! # 用量按追加记录，不维护「当前用量」那一列
//!
//! 维护一列要 `UPDATE`，而 `UPDATE` 在这个项目里只在 redact/purge 里被允许。
//! 更实际的理由是**配额是会被争议的数字**（「我明明没用这么多」），
//! 而只有逐条记录答得上来。当前用量按时间窗聚合算出来 —— 那是一次
//! 走索引的 `sum()`，在这个量级上便宜得多不值得为它引入可变状态。
//!
//! # 自带 key 的用量照记，但不占配额
//!
//! 「我这个月花了多少」与「我还剩多少额度」是两个问题。只记占配额的那些，
//! 第一个问题就永远答不上来了。

use chrono::{Duration, Utc};
use sqlx::Row as _;

use crate::routes::ApiError;
use crate::state::AppState;

/// 配额窗口。
///
/// 自然月会让「这个月」的含义随时区漂移，而滑动窗口不会 —— 它也不会
/// 制造月初那一下集中重置的尖峰。
const WINDOW: Duration = Duration::days(30);

/// 每人每窗口的默认 token 上限。
///
/// 这个数是从**成本**倒推的，不是拍脑袋：按廉价模型每百万 token 一元的
/// 量级算，200 万 token ≈ 每人每月两元。够日常对话用很久，而一次
/// 三年历史导入（实测 408 万 token）会撞上它 —— 那正是我们希望它撞上的
/// 那件事：导入要先跟主人打个招呼。
///
/// 环境变量可调；**设成 0 表示不限**（自托管单人用时那是合理的形态）。
const DEFAULT_LIMIT: i64 = 2_000_000;
const LIMIT_ENV: &str = "CORTEX_QUOTA_TOKENS_PER_MONTH";

/// 这个部署的每人上限。`None` = 不限。
fn limit() -> Option<i64> {
    let raw = std::env::var(LIMIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT);
    (raw > 0).then_some(raw)
}

/// 用量的一条记录。
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// 走的是用户自带的 key 吗。自带的照记，但不占配额。
    pub own_key: bool,
}

/// 当前窗口的用量与上限。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct QuotaStatus {
    /// 占配额的那部分（不含自带 key）
    pub used_tokens: i64,
    /// 自带 key 花掉的。**单独报** —— 它回答的是「我花了多少」而不是
    /// 「我还剩多少」
    pub own_key_tokens: i64,
    /// `None` = 这个部署不限量
    pub limit_tokens: Option<i64>,
    pub window_days: i64,
}

/// 下发给客户端的形状 —— 比 [`QuotaStatus`] 多一个算好的「还剩多少」。
///
/// 剩余不让客户端自己减：`limit - used` 在不限量时该是「无限」而不是负数，
/// 而每个客户端各写一遍这个判断，迟早有一个写成显示 -3999。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct QuotaView {
    pub used_tokens: i64,
    pub own_key_tokens: i64,
    pub limit_tokens: Option<i64>,
    pub remaining_tokens: Option<i64>,
    pub window_days: i64,
}

impl From<QuotaStatus> for QuotaView {
    fn from(s: QuotaStatus) -> Self {
        Self {
            used_tokens: s.used_tokens,
            own_key_tokens: s.own_key_tokens,
            limit_tokens: s.limit_tokens,
            remaining_tokens: s.remaining(),
            window_days: s.window_days,
        }
    }
}

impl QuotaStatus {
    #[must_use]
    pub fn exceeded(&self) -> bool {
        self.limit_tokens.is_some_and(|l| self.used_tokens >= l)
    }

    /// 还剩多少。不限量时是 `None`。
    #[must_use]
    pub fn remaining(&self) -> Option<i64> {
        self.limit_tokens.map(|l| (l - self.used_tokens).max(0))
    }
}

impl AppState {
    /// 记一次用量。
    ///
    /// **失败只记日志，不让业务失败。** 一次对话已经跑完了，为了记不上账
    /// 而把结果丢掉是本末倒置 —— 而漏记的后果只是这一次没被计入配额。
    pub async fn record_usage(&self, user_id: &str, kind: &str, usage: Usage, model: &str) {
        let Ok(acc) = self.accounts() else { return };
        let r = sqlx::query(
            "INSERT INTO cortex_auth.usage
                 (id, user_id, kind, input_tokens, output_tokens, own_key, model)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(cortex_core::Id::new().to_string())
        .bind(user_id)
        .bind(kind)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.own_key)
        .bind(model)
        .execute(&acc.pool)
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, user = user_id, "用量没记上（本次不计入配额）");
        }
    }

    /// 这个用户当前窗口的用量。
    ///
    /// # Errors
    /// 没接数据库，或者查不动。
    pub async fn quota_status(&self, user_id: &str) -> Result<QuotaStatus, ApiError> {
        let since = Utc::now() - WINDOW;
        let row = sqlx::query(
            "SELECT
                 -- ::bigint 是必需的：`sum(bigint)` 在 Postgres 里返回
                 -- **numeric**，而按 i64 取会在解码时 panic —— 表现是
                 -- 连接被掐掉（HTTP 000），不是一条能读的错误。实测撞到过
                 COALESCE(sum(input_tokens + output_tokens) FILTER (WHERE NOT own_key), 0)::bigint AS billed,
                 COALESCE(sum(input_tokens + output_tokens) FILTER (WHERE own_key), 0)::bigint AS own
               FROM cortex_auth.usage
              WHERE user_id = $1 AND occurred_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("算不出用量：{e}")))?;

        Ok(QuotaStatus {
            used_tokens: row.get("billed"),
            own_key_tokens: row.get("own"),
            limit_tokens: limit(),
            window_days: WINDOW.num_days(),
        })
    }

    /// 超了就拦下。
    ///
    /// # 为什么在**发起调用之前**查，而不是事后扣
    ///
    /// 事后扣意味着最后那一次一定会超，而「最后那一次」可能是一次
    /// 六千次调用的导入。查一次是一条走索引的 `sum()`，比一次 LLM 调用
    /// 便宜四个数量级。
    ///
    /// # Errors
    /// 超额（429），或者查不动。
    pub async fn enforce_quota(&self, user_id: &str) -> Result<(), ApiError> {
        // 没接账号体系 = 单用户自托管，花的是自己的钱，不设限
        if self.accounts().is_err() {
            return Ok(());
        }
        let status = self.quota_status(user_id).await?;
        if !status.exceeded() {
            return Ok(());
        }
        // **说清楚超了多少、什么时候恢复。** 一句「配额已用完」会让人
        // 以为账号坏了，然后去重试、去重启、去提 issue
        Err(ApiError::too_many_requests(format!(
            "这个月的用量已经用完了（{} / {} token，按最近 {} 天滚动计算）。\
             等最早那批用量滑出窗口就会自动恢复；\
             要立刻继续，可以在**设置 → 模型 → 自己的 API key**里填一把自己的 ——              自带 key 的调用不占这里的配额",
            status.used_tokens,
            status.limit_tokens.unwrap_or(0),
            status.window_days,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(used: i64, limit: Option<i64>) -> QuotaStatus {
        QuotaStatus {
            used_tokens: used,
            own_key_tokens: 0,
            limit_tokens: limit,
            window_days: 30,
        }
    }

    /// 不限量的部署永远不该被拦。
    ///
    /// 自托管单人用是这个产品的主要形态，而在那里配额只会碍事。
    #[test]
    fn an_unlimited_deployment_never_blocks() {
        assert!(!status(999_999_999, None).exceeded());
        assert_eq!(status(1, None).remaining(), None);
    }

    /// 边界是「用满即拦」，不是「超过才拦」。
    #[test]
    fn the_limit_is_inclusive() {
        assert!(!status(99, Some(100)).exceeded());
        assert!(
            status(100, Some(100)).exceeded(),
            "用满就该拦 —— 放行等于每个人都能多花一次调用的钱"
        );
        assert!(status(101, Some(100)).exceeded());
        assert_eq!(
            status(120, Some(100)).remaining(),
            Some(0),
            "剩余不能是负数"
        );
    }

    /// **自带 key 的用量不占配额，但要能看见。**
    ///
    /// 混进 `used_tokens` 会让填了自己 key 的人照样被拦；
    /// 完全不记则「我这个月花了多少」永远答不上来。
    #[test]
    fn own_key_usage_is_visible_but_not_billed() {
        let s = QuotaStatus {
            used_tokens: 10,
            own_key_tokens: 5_000_000,
            limit_tokens: Some(100),
            window_days: 30,
        };
        assert!(!s.exceeded(), "自带 key 花的钱不该把人拦在自己的配额外面");
        assert_eq!(s.own_key_tokens, 5_000_000, "但它必须看得见");
    }
}
