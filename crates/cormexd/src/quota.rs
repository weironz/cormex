//! Quota: one shared LLM key has to be divided among several people without any
//! single one of them burning it all.
//!
//! # Why this became urgent once registration opened
//!
//! With a single user, "quota" is an empty concept — it is your own money.
//! Once registration is open, the server's key is available to everyone who
//! signs up, and production is a 2-core, 3.5 GB machine: one person running a
//! single three-year history import is 6000 extraction calls.
//!
//! # Usage is recorded append-only; there is no "current usage" column
//!
//! Maintaining such a column requires `UPDATE`, and in this project `UPDATE` is
//! permitted only inside redact/purge. The more practical reason is that **quota
//! is a number people dispute** ("I definitely did not use that much"), and only
//! a per-call record can answer them. Current usage is aggregated over a time
//! window instead — one index-backed `sum()`, which at this scale is far too
//! cheap to justify introducing mutable state for.
//!
//! # Usage on a bring-your-own key is still recorded, but does not count against
//! # the quota
//!
//! "How much did I spend this month" and "how much allowance do I have left" are
//! two different questions. Record only what counts against the quota and the
//! first question becomes permanently unanswerable.

use chrono::{Duration, Utc};
use sqlx::Row as _;

use crate::routes::ApiError;
use crate::state::AppState;

/// The quota window.
///
/// A calendar month makes the meaning of "this month" drift with the timezone;
/// a sliding window does not — and it also avoids the concentrated
/// reset-everyone spike at the start of each month.
const WINDOW: Duration = Duration::days(30);

/// Default per-person token ceiling per window.
///
/// This number is derived backwards from **cost**, not guessed: at the order of
/// one yuan per million tokens for a cheap model, 2 million tokens ≈ two yuan
/// per person per month. That covers everyday conversation for a long time,
/// while a single three-year history import (measured at 4.08 million tokens)
/// runs into it — which is exactly the thing we want it to run into: an import
/// should check with the owner first.
///
/// Adjustable by environment variable; **setting it to 0 means unlimited**
/// (a reasonable shape for a self-hosted, single-person deployment).
const DEFAULT_LIMIT: i64 = 2_000_000;
const LIMIT_ENV: &str = "CORMEX_QUOTA_TOKENS_PER_MONTH";

/// This deployment's per-person ceiling. `None` = unlimited.
fn limit() -> Option<i64> {
    let raw = std::env::var(LIMIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT);
    (raw > 0).then_some(raw)
}

/// One usage record.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Was this served by the user's own key? Own-key usage is still recorded,
    /// but does not count against the quota.
    pub own_key: bool,
}

/// Usage and ceiling for the current window.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct QuotaStatus {
    /// The part that counts against the quota (excludes bring-your-own key)
    pub used_tokens: i64,
    /// Spent on a bring-your-own key. **Reported separately** — it answers
    /// "how much did I spend", not "how much do I have left"
    pub own_key_tokens: i64,
    /// `None` = this deployment is unlimited
    pub limit_tokens: Option<i64>,
    pub window_days: i64,
}

/// The shape sent down to clients — [`QuotaStatus`] plus a pre-computed
/// "how much is left".
///
/// The remainder is not left to the client to subtract: under an unlimited
/// deployment `limit - used` should be "unlimited", not a negative number, and
/// if every client writes that check itself, one of them will eventually end up
/// displaying -3999.
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

    /// How much is left. `None` when unlimited.
    #[must_use]
    pub fn remaining(&self) -> Option<i64> {
        self.limit_tokens.map(|l| (l - self.used_tokens).max(0))
    }
}

impl AppState {
    /// Record one unit of usage.
    ///
    /// **Failure only logs; it never fails the business operation.** A
    /// conversation has already completed by this point, and throwing its result
    /// away because the accounting entry could not be written has the priorities
    /// backwards — the consequence of a missed entry is only that this one call
    /// did not count against the quota.
    pub async fn record_usage(&self, user_id: &str, kind: &str, usage: Usage, model: &str) {
        let Ok(acc) = self.accounts() else { return };
        let r = sqlx::query(
            "INSERT INTO cormex_auth.usage
                 (id, user_id, kind, input_tokens, output_tokens, own_key, model)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(cormex_core::Id::new().to_string())
        .bind(user_id)
        .bind(kind)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .bind(usage.own_key)
        .bind(model)
        .execute(&acc.pool)
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, user = user_id, "the usage was not recorded \
                 (this call does not count against the quota)");
        }
    }

    /// This user's usage over the current window.
    ///
    /// # Errors
    /// No database is attached, or the query failed.
    pub async fn quota_status(&self, user_id: &str) -> Result<QuotaStatus, ApiError> {
        let since = Utc::now() - WINDOW;
        let row = sqlx::query(
            "SELECT
                 -- The ::bigint is required: `sum(bigint)` returns **numeric** in
                 -- Postgres, and reading it back as i64 panics at decode time — which
                 -- shows up as the connection being cut (HTTP 000), not as a readable
                 -- error. Measured, having actually hit it
                 COALESCE(sum(input_tokens + output_tokens) FILTER (WHERE NOT own_key), 0)::bigint AS billed,
                 COALESCE(sum(input_tokens + output_tokens) FILTER (WHERE own_key), 0)::bigint AS own
               FROM cormex_auth.usage
              WHERE user_id = $1 AND occurred_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not compute the usage: {e}")))?;

        Ok(QuotaStatus {
            used_tokens: row.get("billed"),
            own_key_tokens: row.get("own"),
            limit_tokens: limit(),
            window_days: WINDOW.num_days(),
        })
    }

    /// How many tokens this tenant has spent on **distillation** today.
    ///
    /// A calendar day rather than the quota's rolling window: the distiller's
    /// budget is a "don't let one import eat the machine" guard, not an
    /// allowance somebody is entitled to. A rolling window would let it start
    /// spending again the moment yesterday's usage slid out, in the middle of
    /// the import that is the reason the guard exists.
    ///
    /// # Errors
    /// The query failed.
    pub async fn distill_tokens_today(&self, user_id: &str) -> Result<i64, ApiError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(sum(input_tokens + output_tokens), 0)::bigint
               FROM cormex_auth.usage
              WHERE user_id = $1 AND kind = 'distill'
                AND occurred_at >= date_trunc('day', now())",
        )
        .bind(user_id)
        .fetch_one(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not compute the distill usage: {e}")))?;
        Ok(row.0)
    }

    /// Block the caller if they are over the limit.
    ///
    /// # Why the check happens **before** the call, rather than deducting after
    ///
    /// Deducting afterwards means the final call always overshoots, and "the
    /// final call" might be an import of six thousand calls. One check is an
    /// index-backed `sum()`, four orders of magnitude cheaper than one LLM call.
    ///
    /// # Errors
    /// Over quota (429), or the query failed.
    pub async fn enforce_quota(&self, user_id: &str) -> Result<(), ApiError> {
        // No accounts system attached = single-user self-hosting, where the
        // money is your own, so no limit applies
        if self.accounts().is_err() {
            return Ok(());
        }
        let status = self.quota_status(user_id).await?;
        if !status.exceeded() {
            return Ok(());
        }
        // **Say how far over they are and when it recovers.** A bare "quota
        // exhausted" leaves people thinking their account is broken, so they
        // retry, restart, and file an issue
        Err(ApiError::too_many_requests(format!(
            "this window's usage is exhausted ({} / {} tokens, over a rolling {}-day \
             window). It recovers by itself once the oldest usage slides out of the \
             window; to carry on right now, put your own key in under \
             **Settings → Model → Your own API key** — calls on your own key do not \
             count against this quota",
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

    /// An unlimited deployment must never block anyone.
    ///
    /// Single-person self-hosting is this product's primary shape, and there a
    /// quota is nothing but an obstacle.
    #[test]
    fn an_unlimited_deployment_never_blocks() {
        assert!(!status(999_999_999, None).exceeded());
        assert_eq!(status(1, None).remaining(), None);
    }

    /// The boundary is "block on reaching the limit", not "block only after
    /// exceeding it".
    #[test]
    fn the_limit_is_inclusive() {
        assert!(!status(99, Some(100)).exceeded());
        assert!(
            status(100, Some(100)).exceeded(),
            "reaching the limit must block — letting it through means everyone gets to spend \
             one extra call's worth of money"
        );
        assert!(status(101, Some(100)).exceeded());
        assert_eq!(
            status(120, Some(100)).remaining(),
            Some(0),
            "the remainder must never be negative"
        );
    }

    /// **Bring-your-own-key usage does not count against the quota, but it must
    /// still be visible.**
    ///
    /// Folding it into `used_tokens` would block people who supplied their own
    /// key anyway; not recording it at all makes "how much did I spend this
    /// month" permanently unanswerable.
    #[test]
    fn own_key_usage_is_visible_but_not_billed() {
        let s = QuotaStatus {
            used_tokens: 10,
            own_key_tokens: 5_000_000,
            limit_tokens: Some(100),
            window_days: 30,
        };
        assert!(
            !s.exceeded(),
            "money spent on your own key must not lock you out of your own quota"
        );
        assert_eq!(s.own_key_tokens, 5_000_000, "but it must remain visible");
    }
}
